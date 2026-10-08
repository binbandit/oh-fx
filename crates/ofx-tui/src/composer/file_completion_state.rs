use std::ops::Range;

use super::file_picker_path::Query;
use crate::list_window::advance_selection;

pub(crate) const CAPACITY: usize = 32;
pub(crate) const MAX_PATH_LEN: usize = 2048;
const MAX_RAW_QUERY: usize = 2 * MAX_PATH_LEN + 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MentionKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatch {
    pub path: String,
    pub kind: MentionKind,
    pub spans: Vec<Range<usize>>,
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
pub struct IndexRevision {
    pub scope_epoch: u64,
    pub generation: usize,
    pub count: usize,
    pub state: IndexState,
}

impl IndexRevision {
    pub(crate) const READY: Self = Self {
        scope_epoch: 0,
        generation: 0,
        count: 0,
        state: IndexState::Ready,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Loading,
    Ready,
    Empty,
    Unavailable,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    id: u64,
    episode: u64,
    status: Status,
    rows: Vec<FileMatch>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Receipt {
    episode: u64,
    revision: u64,
    pub(crate) selected: Option<usize>,
    window_start: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct View<'a> {
    pub(crate) items: &'a [FileMatch],
    pub(crate) status: Status,
    pub(crate) receipt: Option<Receipt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LookupMode {
    Index,
    Directory,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Anchor {
    raw_query: String,
    raw_len: usize,
    at_offset: usize,
    token_start: usize,
    replace_end: usize,
    quoted: bool,
}

impl Anchor {
    fn matches(&self, query: &Query<'_>, raw: &str) -> bool {
        self.at_offset == query.at_offset
            && self.token_start == query.token_start
            && self.replace_end == query.replace_end
            && self.quoted == query.quoted
            && self.raw_len == query.prefix.len()
            && self.raw_query == raw
    }

    fn assign(&mut self, query: &Query<'_>, raw: &str) {
        raw.clone_into(&mut self.raw_query);
        self.raw_len = query.prefix.len();
        self.at_offset = query.at_offset;
        self.token_start = query.token_start;
        self.replace_end = query.replace_end;
        self.quoted = query.quoted;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct State {
    pub(crate) mode: Option<LookupMode>,
    pub(crate) episode: u64,
    next_revision: u64,
    pub(crate) anchor: Anchor,
    pub(crate) lookup_query: Option<String>,
    source: Option<IndexRevision>,
    pub(crate) directory_request: Option<u64>,
    pub(crate) refresh_requested: bool,
    trusted: bool,
    pub(crate) selection_missing: bool,
    prepared: Option<Snapshot>,
    presented: Option<Snapshot>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            mode: None,
            episode: 1,
            next_revision: 1,
            anchor: Anchor::default(),
            lookup_query: None,
            source: None,
            directory_request: None,
            refresh_requested: false,
            trusted: false,
            selection_missing: false,
            prepared: None,
            presented: None,
        }
    }
}

impl State {
    pub(crate) fn invalidate(&mut self) {
        self.episode = self.episode.wrapping_add(1);
        self.mode = None;
        self.trusted = false;
        self.source = None;
        self.directory_request = None;
        self.refresh_requested = false;
        self.selection_missing = false;
    }

    pub(crate) fn is_active(&self) -> bool {
        self.mode.is_some()
    }

    pub(crate) fn is_indexed(&self) -> bool {
        self.mode != Some(LookupMode::Directory)
    }

    pub(crate) fn distrust(&mut self) {
        self.trusted = false;
    }

    pub(crate) fn reconcile(&mut self, query: Option<&Query<'_>>, indexed: bool) -> bool {
        let Some(query) = query else {
            if self.mode.is_none() {
                return false;
            }
            self.invalidate();
            return true;
        };
        let mode = Some(if indexed {
            LookupMode::Index
        } else {
            LookupMode::Directory
        });
        let raw = bounded_prefix(query.prefix);
        if self.mode == mode && self.anchor.matches(query, raw) {
            return false;
        }
        let lookup = query
            .decoded()
            .filter(|decoded| decoded.len() <= MAX_PATH_LEN);
        let same_lookup = self.mode == mode
            && self.directory_request.is_none()
            && lookup.is_some()
            && self.lookup_query == lookup;
        self.directory_request = None;
        self.refresh_requested = self.refresh_requested || self.mode != mode;
        self.episode = self.episode.wrapping_add(1);
        self.mode = mode;
        self.anchor.assign(query, raw);
        self.trusted = false;
        self.selection_missing = false;
        self.lookup_query = lookup;
        if same_lookup && self.source.is_some() {
            if self.prepared.is_none() {
                self.prepared = self.presented.take();
            }
            if let Some(prepared) = &mut self.prepared {
                prepared.episode = self.episode;
                prepared.id = self.next_revision;
                self.next_revision = self.next_revision.wrapping_add(1);
            }
        } else {
            self.source = None;
        }
        true
    }

    pub(crate) fn needs_lookup(&self, source: IndexRevision) -> bool {
        self.mode.is_some() && self.source != Some(source)
    }

    pub(crate) fn stage(&mut self, source: IndexRevision, status: Status, rows: Vec<FileMatch>) {
        self.prepared = Some(Snapshot {
            id: self.next_revision,
            episode: self.episode,
            status,
            rows,
        });
        self.next_revision = self.next_revision.wrapping_add(1);
        self.source = Some(source);
    }

    fn current(&self) -> Option<&Snapshot> {
        self.presented
            .as_ref()
            .filter(|presented| self.mode.is_some() && presented.episode == self.episode)
    }

    pub(crate) fn selected(&self, index: usize) -> Option<&FileMatch> {
        if !self.trusted || self.selection_missing {
            return None;
        }
        let current = self.current()?;
        if current.status != Status::Ready {
            return None;
        }
        current.rows.get(index)
    }

    pub(crate) fn accepted_empty(&self) -> bool {
        self.current().is_some_and(|current| {
            self.trusted && !self.selection_missing && current.status == Status::Empty
        })
    }

    pub(crate) fn require_lookup(&mut self) {
        self.source = None;
    }

    pub(crate) fn retry(&mut self) {
        self.source = None;
        self.refresh_requested = true;
    }

    pub(crate) fn reject_selection(&mut self) {
        self.selection_missing = true;
    }

    pub(crate) fn navigate(&mut self, index: &mut usize, window: &mut usize, delta: i32) {
        if !self.trusted {
            return;
        }
        let Some(count) = self.current().map(|current| current.rows.len()) else {
            return;
        };
        if count == 0 {
            return;
        }
        if self.selection_missing {
            *index = if delta < 0 { count - 1 } else { 0 };
            *window = 0;
            self.selection_missing = false;
        } else {
            advance_selection(index, window, count, delta);
        }
    }

    pub(crate) fn view(&self, index: usize, window: usize) -> View<'_> {
        let inactive = View {
            items: &[],
            status: Status::Loading,
            receipt: None,
        };
        if self.mode.is_none() {
            return inactive;
        }
        let current = self.current();
        let value = match &self.prepared {
            Some(prepared) if prepared.episode == self.episode => prepared,
            _ => match current {
                Some(current) => current,
                None => return inactive,
            },
        };
        let mut selected = (!self.selection_missing && !value.rows.is_empty()).then_some(0);
        if let Some(old) = current {
            if !old.rows.is_empty() {
                selected = None;
            }
            if !self.selection_missing
                && let Some(choice) = old.rows.get(index)
            {
                selected = value
                    .rows
                    .iter()
                    .position(|item| item.kind == choice.kind && item.path == choice.path)
                    .or(selected);
            }
        }
        let stale = (value.status == Status::Ready && selected.is_none())
            || (value.status == Status::Empty && self.selection_missing);
        View {
            items: &value.rows,
            status: if stale { Status::Stale } else { value.status },
            receipt: Some(Receipt {
                episode: self.episode,
                revision: value.id,
                selected,
                window_start: window,
            }),
        }
    }

    pub(crate) fn acknowledge(&mut self, receipt: Receipt, index: &mut usize, window: &mut usize) {
        if self.mode.is_none() || receipt.episode != self.episode {
            return;
        }
        if self.prepared.as_ref().is_some_and(|prepared| {
            prepared.id == receipt.revision && prepared.episode == receipt.episode
        }) {
            self.presented = self.prepared.take();
        }
        let Some(current) = self.current() else {
            return;
        };
        if current.id != receipt.revision {
            return;
        }
        let has_rows = !current.rows.is_empty();
        self.selection_missing = self.selection_missing || (receipt.selected.is_none() && has_rows);
        *index = receipt.selected.unwrap_or(0);
        *window = receipt.window_start;
        self.trusted = true;
    }
}

fn bounded_prefix(query: &str) -> &str {
    &query[..query.floor_char_boundary(MAX_RAW_QUERY)]
}

#[cfg(test)]
mod tests {
    use super::super::file_picker_path::query_at;
    use super::*;

    fn row(path: &str) -> FileMatch {
        FileMatch {
            path: path.to_owned(),
            kind: MentionKind::File,
            spans: Vec::new(),
        }
    }

    fn rows(paths: &[&str]) -> Vec<FileMatch> {
        paths.iter().map(|path| row(path)).collect()
    }

    fn present(state: &mut State, index: &mut usize, window: &mut usize) {
        let receipt = state.view(*index, *window).receipt.unwrap();
        state.acknowledge(receipt, index, window);
    }

    #[test]
    fn prepared_rows_cannot_replace_the_presented_identity_before_acknowledgement() {
        let mut state = State::default();
        let query = query_at("@./", 3).unwrap();
        state.reconcile(Some(&query), false);
        state.stage(
            IndexRevision::default(),
            Status::Ready,
            rows(&["./b.txt", "./c.txt"]),
        );
        let (mut index, mut window) = (0, 0);
        assert_eq!(state.selected(index), None);
        present(&mut state, &mut index, &mut window);
        state.navigate(&mut index, &mut window, 1);
        state.stage(
            IndexRevision::default(),
            Status::Ready,
            rows(&["./a.txt", "./b.txt", "./c.txt"]),
        );
        assert_eq!(state.selected(index).unwrap().path, "./c.txt");
        let receipt = state.view(index, window).receipt.unwrap();
        assert_eq!(receipt.selected, Some(2));
        state.distrust();
        assert_eq!(state.selected(index), None);
        state.acknowledge(receipt, &mut index, &mut window);
        assert_eq!(state.selected(index).unwrap().path, "./c.txt");
        state.invalidate();
        state.reconcile(Some(&query), false);
        state.acknowledge(receipt, &mut index, &mut window);
        assert_eq!(state.selected(index), None);
    }

    #[test]
    fn a_missing_selection_requires_navigation_and_retry_never_acknowledges() {
        let mut state = State::default();
        state.reconcile(query_at("@.", 2).as_ref(), false);
        let (mut index, mut window) = (0, 0);
        state.stage(IndexRevision::default(), Status::Ready, rows(&["./a"]));
        present(&mut state, &mut index, &mut window);
        state.stage(IndexRevision::default(), Status::Ready, rows(&["./b"]));
        let view = state.view(index, window);
        assert_eq!(view.status, Status::Stale);
        let receipt = view.receipt.unwrap();
        assert_eq!(state.selected(index).unwrap().path, "./a");
        state.acknowledge(receipt, &mut index, &mut window);
        assert_eq!(state.selected(index), None);
        state.navigate(&mut index, &mut window, 1);
        assert_eq!(state.selected(index).unwrap().path, "./b");
        state.stage(IndexRevision::default(), Status::Unavailable, Vec::new());
        present(&mut state, &mut index, &mut window);
        state.retry();
        assert!(!state.accepted_empty());
        assert_eq!(state.selected(index), None);
        state.stage(IndexRevision::default(), Status::Ready, rows(&["./b"]));
        present(&mut state, &mut index, &mut window);
        assert_eq!(state.selected(index).unwrap().path, "./b");
        state.stage(IndexRevision::default(), Status::Empty, Vec::new());
        assert!(!state.accepted_empty());
        present(&mut state, &mut index, &mut window);
        assert!(state.accepted_empty());
    }

    #[test]
    fn a_rejected_selection_stays_unselected_through_loading_and_retry() {
        let mut state = State::default();
        state.reconcile(query_at("@./", 3).as_ref(), false);
        let (mut index, mut window) = (0, 0);
        state.stage(
            IndexRevision::default(),
            Status::Ready,
            rows(&["./b.txt", "./c.txt"]),
        );
        present(&mut state, &mut index, &mut window);
        state.navigate(&mut index, &mut window, 1);
        assert_eq!(state.selected(index).unwrap().path, "./c.txt");
        state.reject_selection();
        state.retry();
        for status in [
            Status::Loading,
            Status::Unavailable,
            Status::Empty,
            Status::Loading,
        ] {
            state.stage(IndexRevision::default(), status, Vec::new());
            present(&mut state, &mut index, &mut window);
            assert_eq!(state.selected(index), None);
            assert!(!state.accepted_empty());
        }
        state.stage(IndexRevision::default(), Status::Ready, rows(&["./b.txt"]));
        let retry_view = state.view(index, window);
        assert_eq!(retry_view.status, Status::Stale);
        assert_eq!(retry_view.receipt.unwrap().selected, None);
        present(&mut state, &mut index, &mut window);
        assert_eq!(state.selected(index), None);
        state.navigate(&mut index, &mut window, 1);
        assert_eq!(state.selected(index).unwrap().path, "./b.txt");
    }

    #[test]
    fn another_occurrence_or_cursor_position_invalidates_identical_query_authority() {
        let mut state = State::default();
        let input = "@foo @foo";
        state.reconcile(query_at(input, 4).as_ref(), true);
        let source = IndexRevision {
            generation: 1,
            state: IndexState::Ready,
            ..IndexRevision::default()
        };
        state.stage(source, Status::Empty, Vec::new());
        let (mut index, mut window) = (0, 0);
        let receipt = state.view(index, window).receipt.unwrap();
        state.acknowledge(receipt, &mut index, &mut window);
        assert!(state.accepted_empty());
        assert!(!state.reconcile(query_at(input, 4).as_ref(), true));
        assert!(!state.needs_lookup(source));
        state.distrust();
        assert!(!state.needs_lookup(source));
        assert!(state.reconcile(query_at(input, input.len()).as_ref(), true));
        assert!(!state.needs_lookup(source));
        state.acknowledge(receipt, &mut index, &mut window);
        assert!(!state.accepted_empty());
        assert!(state.reconcile(None, true));
        assert!(!state.is_active());
        assert!(!state.reconcile(None, true));
    }
}
