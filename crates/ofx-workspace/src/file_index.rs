mod discovery;
mod matcher;

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use rustix::fs::{AtFlags, FileType, statat};

use crate::file_index_cache;
use crate::pathing::path_inside;

pub(crate) use matcher::NameQuery;

pub(crate) const MAX_INDEXED_FILES: usize = 100_000;
pub const MAX_PATH_LEN: usize = 2048;
pub(crate) const MAX_SEARCH_RESULTS: usize = 64;
const LOADER_THREAD: &str = "oh-fx-file-index";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) path: String,
    pub(crate) kind: CandidateKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub path: String,
    pub kind: CandidateKind,
    pub matched_spans: Vec<Range<usize>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IndexState {
    #[default]
    Idle,
    Loading,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadableRevision {
    pub scope_epoch: u64,
    pub generation: usize,
    pub count: usize,
    pub state: IndexState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("InvalidIndexData")]
pub struct InvalidIndexData;

struct Generation {
    id: usize,
    scope_epoch: u64,
    from_cache: bool,
    paths: String,
    lower: Vec<u8>,
    offsets: Vec<usize>,
    basename_starts: Vec<usize>,
    kinds: Vec<CandidateKind>,
    masks: Vec<u32>,
}

impl Generation {
    fn build(
        id: usize,
        scope_epoch: u64,
        candidates: &[Candidate],
        stop: Option<&AtomicBool>,
    ) -> Option<Self> {
        let mut generation = Self {
            id,
            scope_epoch,
            from_cache: false,
            paths: String::new(),
            lower: Vec::new(),
            offsets: vec![0],
            basename_starts: Vec::new(),
            kinds: Vec::new(),
            masks: Vec::new(),
        };
        for candidate in candidates
            .iter()
            .filter(|candidate| accepted_candidate(candidate))
            .take(MAX_INDEXED_FILES)
        {
            if stop.is_some_and(|stop| stop.load(Ordering::SeqCst)) {
                return None;
            }
            generation.push(candidate);
        }
        Some(generation)
    }

    fn push(&mut self, candidate: &Candidate) {
        let start = self.paths.len();
        let path = candidate.path.as_str();
        self.paths.push_str(path);
        self.lower
            .extend(path.bytes().map(|byte| byte.to_ascii_lowercase()));
        self.basename_starts
            .push(start + path.rfind('/').map_or(0, |slash| slash + 1));
        self.masks.push(matcher::path_mask(path));
        self.kinds.push(candidate.kind);
        self.offsets.push(self.paths.len());
    }

    fn count(&self) -> usize {
        self.kinds.len()
    }

    fn path_at(&self, index: usize) -> &str {
        &self.paths[self.offsets[index]..self.offsets[index + 1]]
    }

    fn lower_path_at(&self, index: usize) -> &[u8] {
        &self.lower[self.offsets[index]..self.offsets[index + 1]]
    }

    fn basename_offset(&self, index: usize) -> usize {
        self.basename_starts[index] - self.offsets[index]
    }
}

enum LoaderOutcome {
    Ready(Generation),
    Failed,
    Canceled,
}

struct Loader {
    id: usize,
    done: Arc<AtomicBool>,
    thread: JoinHandle<LoaderOutcome>,
}

pub struct FileIndex {
    root: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    scope_epoch: u64,
    active: Option<Generation>,
    loader: Option<Loader>,
    refresh_pending: bool,
    stop: Arc<AtomicBool>,
    generation: usize,
    initial_failed: bool,
    cache_attempted: bool,
}

impl FileIndex {
    pub fn new(cache_dir: Option<PathBuf>) -> Self {
        Self {
            root: None,
            cache_dir,
            scope_epoch: 0,
            active: None,
            loader: None,
            refresh_pending: false,
            stop: Arc::new(AtomicBool::new(false)),
            generation: 0,
            initial_failed: false,
            cache_attempted: false,
        }
    }

    pub fn ensure_scope(&mut self, root: &Path) {
        if self.current_state() != IndexState::Idle || root.as_os_str().is_empty() {
            return;
        }
        self.root = Some(root.to_owned());
        self.start_load();
    }

    pub fn refresh(&mut self) {
        if self.stop.load(Ordering::SeqCst) {
            return;
        }
        if self.loader.is_some() {
            self.refresh_pending = true;
            return;
        }
        self.start_load();
    }

    pub fn current_state(&self) -> IndexState {
        if self.active.is_some() {
            return IndexState::Ready;
        }
        if self.loader.is_some() {
            return IndexState::Loading;
        }
        if self.initial_failed {
            IndexState::Failed
        } else {
            IndexState::Idle
        }
    }

    pub fn is_loading(&self) -> bool {
        self.loader.is_some()
    }

    pub fn join_if_done(&mut self) -> bool {
        let Some(loader) = self
            .loader
            .take_if(|loader| loader.done.load(Ordering::Acquire))
        else {
            return false;
        };
        let outcome = loader.thread.join().unwrap_or(LoaderOutcome::Failed);
        let mut visible_changed = false;
        let mut adopted_from_cache = false;
        match outcome {
            LoaderOutcome::Ready(generation) if generation.id == loader.id => {
                adopted_from_cache = generation.from_cache;
                self.active = Some(generation);
                self.initial_failed = false;
                visible_changed = true;
            }
            LoaderOutcome::Ready(_) | LoaderOutcome::Canceled => {}
            LoaderOutcome::Failed => {
                if self.active.is_none() {
                    self.initial_failed = true;
                    visible_changed = true;
                }
            }
        }
        if !self.stop.load(Ordering::SeqCst)
            && (std::mem::take(&mut self.refresh_pending) || adopted_from_cache)
        {
            self.start_load();
        }
        visible_changed
    }

    pub fn readable_revision(&self) -> ReadableRevision {
        let state = self.current_state();
        match &self.active {
            Some(generation) => ReadableRevision {
                scope_epoch: generation.scope_epoch,
                generation: generation.id,
                count: generation.count(),
                state,
            },
            None => ReadableRevision {
                scope_epoch: self.scope_epoch,
                state,
                ..ReadableRevision::default()
            },
        }
    }

    pub fn search_at_revision(
        &self,
        revision: ReadableRevision,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, InvalidIndexData> {
        if limit == 0 || revision.count == 0 {
            return Ok(Vec::new());
        }
        let generation = self.active.as_ref().ok_or(InvalidIndexData)?;
        if generation.id != revision.generation
            || generation.scope_epoch != revision.scope_epoch
            || revision.count > generation.count()
        {
            return Err(InvalidIndexData);
        }
        if query.len() > MAX_PATH_LEN {
            return Ok(Vec::new());
        }
        if query.is_empty() {
            return Ok((0..revision.count.min(limit))
                .map(|index| SearchResult {
                    path: generation.path_at(index).to_owned(),
                    kind: generation.kinds[index],
                    matched_spans: Vec::new(),
                })
                .collect());
        }
        let Some(prepared) = matcher::PreparedQuery::new(query) else {
            return Ok(Vec::new());
        };
        matcher::rank_top(
            generation,
            revision.count,
            &prepared,
            limit.min(MAX_SEARCH_RESULTS),
        )
        .into_iter()
        .map(|index| {
            let path = generation.path_at(index);
            let spans = matcher::match_spans(path, generation.basename_offset(index), &prepared)
                .ok_or(InvalidIndexData)?;
            Ok(SearchResult {
                path: path.to_owned(),
                kind: generation.kinds[index],
                matched_spans: spans,
            })
        })
        .collect()
    }

    pub fn is_current_candidate_kind(&self, path: &str, expected: CandidateKind) -> bool {
        let Some(root) = &self.root else {
            return false;
        };
        if !is_terminal_safe(path) {
            return false;
        }
        let resolved = if path.starts_with('/') {
            if !path_inside(root, Path::new(path)) {
                return false;
            }
            PathBuf::from(path)
        } else {
            root.join(path)
        };
        statat(rustix::fs::CWD, &resolved, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|stat| kind_matches(expected, FileType::from_raw_mode(stat.st_mode)))
    }

    fn start_load(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        if self.loader.is_some() || self.stop.load(Ordering::SeqCst) {
            return;
        }
        let allow_cache = !std::mem::replace(&mut self.cache_attempted, true);
        let id = self.generation + 1;
        let done = Arc::new(AtomicBool::new(false));
        let job = LoadJob {
            id,
            scope_epoch: self.scope_epoch,
            root,
            cache_dir: self.cache_dir.clone(),
            allow_cache,
            stop: Arc::clone(&self.stop),
            done: Arc::clone(&done),
        };
        match thread::Builder::new()
            .name(LOADER_THREAD.to_owned())
            .spawn(move || job.run())
        {
            Ok(thread) => {
                self.generation = id;
                self.initial_failed = false;
                self.loader = Some(Loader { id, done, thread });
            }
            Err(_) => {
                if self.active.is_none() {
                    self.initial_failed = true;
                }
            }
        }
    }
}

impl Drop for FileIndex {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

struct LoadJob {
    id: usize,
    scope_epoch: u64,
    root: PathBuf,
    cache_dir: Option<PathBuf>,
    allow_cache: bool,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl LoadJob {
    fn run(self) -> LoaderOutcome {
        let outcome = self.load();
        self.done.store(true, Ordering::Release);
        outcome
    }

    fn load(&self) -> LoaderOutcome {
        if self.stopped() {
            return LoaderOutcome::Canceled;
        }
        let roots = [self.root.as_path()];
        if self.allow_cache
            && let Some(cache_dir) = &self.cache_dir
            && let Some(candidates) = file_index_cache::load(cache_dir, &roots)
        {
            return match Generation::build(self.id, self.scope_epoch, &candidates, Some(&self.stop))
            {
                Some(mut generation) => {
                    generation.from_cache = true;
                    LoaderOutcome::Ready(generation)
                }
                None => LoaderOutcome::Canceled,
            };
        }
        let candidates = match discovery::discover_scope(&self.root, &self.stop) {
            Ok(candidates) => candidates,
            Err(discovery::DiscoveryError::Canceled) => return LoaderOutcome::Canceled,
            Err(discovery::DiscoveryError::Failed) => {
                return if self.stopped() {
                    LoaderOutcome::Canceled
                } else {
                    LoaderOutcome::Failed
                };
            }
        };
        if let Some(cache_dir) = &self.cache_dir {
            let _ = file_index_cache::save(cache_dir, &roots, &candidates);
        }
        match Generation::build(self.id, self.scope_epoch, &candidates, Some(&self.stop)) {
            Some(generation) if !self.stopped() => LoaderOutcome::Ready(generation),
            _ => LoaderOutcome::Canceled,
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

pub(crate) fn is_terminal_safe(path: &str) -> bool {
    path.chars().all(ofx_text::is_terminal_safe_char)
}

pub(crate) fn accepted_candidate(candidate: &Candidate) -> bool {
    !candidate.path.is_empty()
        && candidate.path.len() <= MAX_PATH_LEN
        && is_terminal_safe(&candidate.path)
        && !(candidate.kind == CandidateKind::Directory && candidate.path.ends_with('/'))
}

fn kind_matches(expected: CandidateKind, actual: FileType) -> bool {
    match expected {
        CandidateKind::File => matches!(actual, FileType::RegularFile | FileType::Symlink),
        CandidateKind::Directory => actual == FileType::Directory,
    }
}

#[cfg(test)]
mod tests;
