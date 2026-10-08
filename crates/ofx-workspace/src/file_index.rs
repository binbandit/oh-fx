mod discovery;
mod matcher;

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use ofx_text::is_terminal_safe;
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

struct IndexedPath {
    start: u32,
    end: u32,
    basename: u32,
    mask: u32,
    kind: CandidateKind,
}

impl IndexedPath {
    fn range(&self) -> Range<usize> {
        self.start as usize..self.end as usize
    }

    fn basename_offset(&self) -> usize {
        self.basename as usize
    }
}

struct Generation {
    id: usize,
    scope_epoch: u64,
    from_cache: bool,
    paths: String,
    lower: Vec<u8>,
    entries: Vec<IndexedPath>,
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
            entries: Vec::new(),
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
        let path = candidate.path.as_str();
        let (Ok(start), Ok(end), Ok(basename)) = (
            u32::try_from(self.paths.len()),
            u32::try_from(self.paths.len() + path.len()),
            u32::try_from(path.rfind('/').map_or(0, |slash| slash + 1)),
        ) else {
            return;
        };
        self.paths.push_str(path);
        self.lower
            .extend(path.bytes().map(|byte| byte.to_ascii_lowercase()));
        self.entries.push(IndexedPath {
            start,
            end,
            basename,
            mask: matcher::path_mask(path),
            kind: candidate.kind,
        });
    }

    fn count(&self) -> usize {
        self.entries.len()
    }

    fn path(&self, entry: &IndexedPath) -> &str {
        self.paths.get(entry.range()).unwrap_or_default()
    }

    fn lower_path(&self, entry: &IndexedPath) -> &[u8] {
        self.lower.get(entry.range()).unwrap_or_default()
    }

    fn path_at(&self, index: usize) -> &str {
        self.entries.get(index).map_or("", |entry| self.path(entry))
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

struct PendingScope {
    roots: Vec<PathBuf>,
    epoch: u64,
}

pub struct FileIndex {
    roots: Vec<PathBuf>,
    cache_dir: Option<PathBuf>,
    scope_epoch: u64,
    active: Option<Generation>,
    loader: Option<Loader>,
    pending_scope: Option<PendingScope>,
    stop: Arc<AtomicBool>,
    saving: Arc<Mutex<()>>,
    generation: usize,
    initial_failed: bool,
    cache_attempted_roots: Option<Vec<PathBuf>>,
}

impl FileIndex {
    pub fn new(cache_dir: Option<PathBuf>) -> Self {
        Self {
            roots: Vec::new(),
            cache_dir,
            scope_epoch: 0,
            active: None,
            loader: None,
            pending_scope: None,
            stop: Arc::new(AtomicBool::new(false)),
            saving: Arc::new(Mutex::new(())),
            generation: 0,
            initial_failed: false,
            cache_attempted_roots: None,
        }
    }

    pub fn ensure_scope(&mut self, primary: &Path, additional: &[PathBuf]) {
        if self.current_state() != IndexState::Idle || primary.as_os_str().is_empty() {
            return;
        }
        self.roots = scope_roots(primary, additional);
        self.start_load();
    }

    pub fn refresh(&mut self) {
        let (roots, epoch) = match &self.pending_scope {
            Some(pending) => (pending.roots.clone(), pending.epoch),
            None => (self.roots.clone(), self.scope_epoch),
        };
        self.refresh_roots(roots, epoch);
    }

    pub fn refresh_scope(&mut self, primary: &Path, additional: &[PathBuf], epoch: u64) {
        self.refresh_roots(scope_roots(primary, additional), epoch);
    }

    fn refresh_roots(&mut self, roots: Vec<PathBuf>, epoch: u64) {
        if self.stop.load(Ordering::SeqCst) {
            return;
        }
        if self.loader.is_some() {
            self.pending_scope = Some(PendingScope { roots, epoch });
            return;
        }
        self.roots = roots;
        self.scope_epoch = epoch;
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
        if !self.stop.load(Ordering::SeqCst) {
            if let Some(pending) = self.pending_scope.take() {
                self.roots = pending.roots;
                self.scope_epoch = pending.epoch;
                self.start_load();
            }
            if adopted_from_cache {
                self.start_load();
            }
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
            return Ok(generation
                .entries
                .iter()
                .take(revision.count.min(limit))
                .map(|entry| SearchResult {
                    path: generation.path(entry).to_owned(),
                    kind: entry.kind,
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
            let entry = generation.entries.get(index).ok_or(InvalidIndexData)?;
            let path = generation.path(entry);
            let spans = matcher::match_spans(path, entry.basename_offset(), &prepared)
                .ok_or(InvalidIndexData)?;
            Ok(SearchResult {
                path: path.to_owned(),
                kind: entry.kind,
                matched_spans: spans,
            })
        })
        .collect()
    }

    pub fn is_current_candidate_kind(&self, path: &str, expected: CandidateKind) -> bool {
        let roots = self
            .pending_scope
            .as_ref()
            .map_or(&self.roots, |pending| &pending.roots);
        let Some(primary) = roots.first() else {
            return false;
        };
        if !is_terminal_safe(path.as_bytes()) {
            return false;
        }
        let resolved = if path.starts_with('/') {
            if !roots.iter().any(|root| path_inside(root, Path::new(path))) {
                return false;
            }
            PathBuf::from(path)
        } else {
            primary.join(path)
        };
        statat(rustix::fs::CWD, &resolved, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|stat| kind_matches(expected, FileType::from_raw_mode(stat.st_mode)))
    }

    fn start_load(&mut self) {
        if self.roots.is_empty() {
            return;
        }
        if self.loader.is_some() || self.stop.load(Ordering::SeqCst) {
            return;
        }
        let allow_cache = self.cache_attempted_roots.as_ref() != Some(&self.roots);
        if allow_cache {
            self.cache_attempted_roots = Some(self.roots.clone());
        }
        let id = self.generation + 1;
        let done = Arc::new(AtomicBool::new(false));
        let job = LoadJob {
            id,
            scope_epoch: self.scope_epoch,
            roots: self.roots.clone(),
            cache_dir: self.cache_dir.clone(),
            allow_cache,
            stop: Arc::clone(&self.stop),
            saving: Arc::clone(&self.saving),
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

fn scope_roots(primary: &Path, additional: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = vec![primary.to_owned()];
    for root in additional {
        if !roots.contains(root) {
            roots.push(root.clone());
        }
    }
    roots
}

impl Drop for FileIndex {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        drop(self.saving.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

struct LoadJob {
    id: usize,
    scope_epoch: u64,
    roots: Vec<PathBuf>,
    cache_dir: Option<PathBuf>,
    allow_cache: bool,
    stop: Arc<AtomicBool>,
    saving: Arc<Mutex<()>>,
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
        let roots: Vec<&Path> = self.roots.iter().map(PathBuf::as_path).collect();
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
        let candidates = match discovery::discover_scope(&self.roots, &self.stop) {
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
            let _saving = self.saving.lock().unwrap_or_else(PoisonError::into_inner);
            if self.stopped() {
                return LoaderOutcome::Canceled;
            }
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

pub(crate) fn accepted_candidate(candidate: &Candidate) -> bool {
    !candidate.path.is_empty()
        && candidate.path.len() <= MAX_PATH_LEN
        && is_terminal_safe(candidate.path.as_bytes())
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
