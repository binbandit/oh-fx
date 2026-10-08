use super::Shell;
use super::directory_completion_job::{DirectoryCompletionJob, DirectoryLister};
use crate::composer::file_completion_state::{
    CAPACITY, FileMatch, IndexRevision, IndexState, MAX_PATH_LEN, MentionKind, Receipt, State,
    Status,
};
use crate::composer::file_picker_path::{
    EncodeOptions, Query, encode, is_representable, is_separator, query_at,
};
use crate::composer::{Composer, EditRange, InsertResult, projected_anchor_column};
use crate::footer::picker_presentation::{
    FilePickerFrame, FilePickerStatus, file_picker_band, list_picker_rows,
};
use crate::input::COMPOSER_INPUT_LIMIT_BYTES;
use crate::row_text::Row;

const FILES_UNAVAILABLE: &str = "Files unavailable. tab to retry; esc to dismiss.";
const DIRECTORY_UNAVAILABLE: &str = "Directory unavailable. tab to retry; esc to dismiss.";
const SELECTION_UNAVAILABLE: &str = "Selection unavailable. navigate to choose; tab to retry.";
const PICKER_PREFIXES: [&str; 4] = ["/model ", "/provider ", "/login ", "/setup "];
const MCP_COMMAND: &str = "/mcp";
const COMMAND_WHITESPACE: [char; 4] = [' ', '\t', '\r', '\n'];

pub trait FileMentionSource {
    fn revision(&self) -> IndexRevision;
    fn search(&self, revision: IndexRevision, query: &str, limit: usize) -> Option<Vec<FileMatch>>;
    fn refresh(&mut self);
    fn poll(&mut self) -> bool;
    fn is_loading(&self) -> bool;
    fn depends_on_index(&self, query: &str) -> bool;
    fn is_current(&self, query: &str, path: &str, kind: MentionKind) -> bool;
    fn directory_lister(&self) -> DirectoryLister;
}

pub(super) struct FilePicker {
    source: Option<Box<dyn FileMentionSource>>,
    state: State,
    index: usize,
    window_start: usize,
    episode_seen: bool,
    suppressed: bool,
    job: DirectoryCompletionJob,
}

struct Selection {
    at_offset: usize,
    replace_end: usize,
    quoted: bool,
    decoded: String,
    choice: FileMatch,
}

#[derive(Default)]
pub(super) struct PickerBand {
    pub(super) rows: Vec<Row>,
    pub(super) receipt: Option<Receipt>,
}

impl FilePicker {
    pub(super) fn new(source: Option<Box<dyn FileMentionSource>>) -> Self {
        let lister = source.as_ref().map(|source| source.directory_lister());
        Self {
            source,
            state: State::default(),
            index: 0,
            window_start: 0,
            episode_seen: false,
            suppressed: false,
            job: DirectoryCompletionJob::new(lister),
        }
    }

    fn query<'c>(&self, composer: &'c Composer) -> Option<Query<'c>> {
        if self.source.is_none() || self.suppressed {
            return None;
        }
        raw_file_query(composer.text(), composer.cursor())
    }

    fn depends_on_index(&self, query: &Query<'_>) -> bool {
        match (&self.source, query.decoded()) {
            (Some(source), Some(decoded)) => source.depends_on_index(&decoded),
            _ => true,
        }
    }

    fn reconcile(&mut self, composer: &Composer, eligible: bool, distrusted: bool) {
        let query = self.query(composer);
        let indexed = query
            .as_ref()
            .is_none_or(|query| self.depends_on_index(query));
        if self.state.reconcile(query.as_ref(), indexed) {
            self.reset_index();
        }
        if !eligible || distrusted {
            self.state.distrust();
        }
        self.job.reconcile(&mut self.state, eligible);
    }

    fn reset_index(&mut self) {
        self.index = 0;
        self.window_start = 0;
    }

    fn prepare(&mut self, composer: &Composer) -> bool {
        if !self.state.is_active() {
            return false;
        }
        let Some(source) = self.source.as_mut() else {
            return false;
        };
        if std::mem::take(&mut self.state.refresh_requested) && self.state.is_indexed() {
            let first_episode = !std::mem::replace(&mut self.episode_seen, true);
            if !first_episode || !source.is_loading() {
                source.refresh();
            }
        }
        let revision = if self.state.is_indexed() {
            source.revision()
        } else {
            IndexRevision::READY
        };
        if !self.state.needs_lookup(revision) {
            return false;
        }
        let Some(query) = self.query(composer) else {
            return false;
        };
        let Some(decoded) = query
            .decoded()
            .filter(|decoded| decoded.len() <= MAX_PATH_LEN)
        else {
            self.state.stage(revision, Status::Unavailable, Vec::new());
            return true;
        };
        if !self.state.is_indexed() {
            self.job.schedule(&mut self.state);
            return true;
        }
        let Some(source) = self.source.as_ref() else {
            return false;
        };
        match source.search(revision, &decoded, CAPACITY) {
            Some(mut rows) => {
                rows.truncate(CAPACITY);
                let status = if rows.is_empty() {
                    match revision.state {
                        IndexState::Loading => Status::Loading,
                        IndexState::Failed => Status::Unavailable,
                        IndexState::Idle | IndexState::Ready => Status::Empty,
                    }
                } else {
                    Status::Ready
                };
                self.state.stage(revision, status, rows);
            }
            None => self.state.stage(revision, Status::Unavailable, Vec::new()),
        }
        true
    }

    fn selection(&self, composer: &Composer) -> Option<Selection> {
        let query = self.query(composer)?;
        let choice = self.state.selected(self.index)?.clone();
        Some(Selection {
            at_offset: query.at_offset,
            replace_end: query.replace_end,
            quoted: query.quoted,
            decoded: query.decoded()?,
            choice,
        })
    }

    fn apply(&mut self, composer: &mut Composer, selection: &Selection) -> InsertResult {
        let Some(source) = self.source.as_ref() else {
            return InsertResult::Inactive;
        };
        let path = selection.choice.path.as_str();
        if !is_representable(path) {
            self.reset_index();
            return InsertResult::Inactive;
        }
        let indexed = source.depends_on_index(&selection.decoded);
        if !source.is_current(&selection.decoded, path, selection.choice.kind) {
            self.state.reject_selection();
            return InsertResult::Inactive;
        }
        let reuses_terminator = composer
            .text()
            .as_bytes()
            .get(selection.replace_end)
            .copied()
            .is_some_and(is_separator);
        let directory = selection.choice.kind == MentionKind::Directory;
        let adds_terminator = !directory && !reuses_terminator;
        let Some(mut replacement) = encode(
            path,
            EncodeOptions {
                quoted: selection.quoted,
                directory,
                workspace_relative: indexed,
            },
        ) else {
            return InsertResult::Inactive;
        };
        let separator_offset = selection.at_offset + replacement.len();
        if adds_terminator {
            replacement.push(' ');
        }
        let cursor_after =
            selection.at_offset + replacement.len() + usize::from(!directory && reuses_terminator);
        let result = composer.replace_range_bounded(
            EditRange {
                start: selection.at_offset,
                end: selection.replace_end,
            },
            &replacement,
            cursor_after,
            COMPOSER_INPUT_LIMIT_BYTES,
        );
        if result == InsertResult::Inserted {
            if adds_terminator {
                composer.mark_auto_separator(separator_offset);
            }
            if directory {
                self.reset_index();
            }
        }
        result
    }

    fn status(&self, status: Status) -> FilePickerStatus<'static> {
        match status {
            Status::Unavailable if self.state.is_indexed() => {
                FilePickerStatus::Notice(FILES_UNAVAILABLE)
            }
            Status::Unavailable => FilePickerStatus::Notice(DIRECTORY_UNAVAILABLE),
            Status::Stale => FilePickerStatus::Notice(SELECTION_UNAVAILABLE),
            Status::Loading => FilePickerStatus::Loading,
            Status::Ready => FilePickerStatus::Rows,
            Status::Empty => FilePickerStatus::Empty,
        }
    }
}

impl Shell<'_> {
    fn file_picker_eligible(&self) -> bool {
        self.approval.is_none()
            && self.question.is_none()
            && !self.skills_menu_visible()
            && self.model_menu.is_none()
            && self.help_menu.is_none()
            && self.picker.is_none()
            && !self.full_transcript_open()
    }

    fn file_picker_distrusted(&self) -> bool {
        self.dimensions_invalid || self.pending_resize.is_some()
    }

    pub(super) fn has_file_query(&self) -> bool {
        self.file_picker.query(&self.composer).is_some()
    }

    fn file_picker_owns_surface(&self) -> bool {
        self.file_picker_eligible() && self.has_file_query()
    }

    pub(super) fn reconcile_file_picker(&mut self) {
        let eligible = self.file_picker_eligible();
        let distrusted = self.file_picker_distrusted();
        self.file_picker
            .reconcile(&self.composer, eligible, distrusted);
    }

    pub(super) fn collect_file_picker_facts(&mut self) -> bool {
        let polled = self
            .file_picker
            .source
            .as_mut()
            .is_some_and(|source| source.poll());
        self.reconcile_file_picker();
        let eligible = self.file_picker_eligible();
        let picker = &mut self.file_picker;
        let harvested = picker.job.harvest(&mut picker.state, eligible);
        polled || harvested
    }

    pub(super) fn prepare_file_picker(&mut self) -> bool {
        self.reconcile_file_picker();
        self.file_picker_eligible() && self.file_picker.prepare(&self.composer)
    }

    pub(super) fn file_picker_busy(&self) -> bool {
        self.file_picker_owns_surface()
            && (self.file_picker.job.is_busy()
                || self
                    .file_picker
                    .source
                    .as_ref()
                    .is_some_and(|source| source.is_loading()))
    }

    pub(super) fn file_picker_band(&self, input_extra: usize, banner_rows: usize) -> PickerBand {
        if !self.file_picker_eligible() {
            return PickerBand::default();
        }
        let Some(query) = self.file_picker.query(&self.composer) else {
            return PickerBand::default();
        };
        let picker = &self.file_picker;
        let view = picker.state.view(picker.index, picker.window_start);
        let summary = self
            .composer
            .visual_layout(self.layout.cols)
            .summary(Some(query.at_offset));
        let frame = FilePickerFrame {
            items: view.items,
            selected: view.receipt.and_then(|receipt| receipt.selected),
            window_start: picker.window_start,
            status: picker.status(view.status),
            start_col: usize::from(projected_anchor_column(summary, self.layout.cols)),
            cols: self.cols(),
            rows: list_picker_rows(usize::from(self.layout.rows), input_extra, banner_rows),
        };
        PickerBand {
            rows: file_picker_band(&self.theme, &frame),
            receipt: view.receipt,
        }
    }

    pub(super) fn acknowledge_file_picker(&mut self, receipt: Receipt) {
        if !self.file_picker_eligible() {
            return;
        }
        let picker = &mut self.file_picker;
        picker
            .state
            .acknowledge(receipt, &mut picker.index, &mut picker.window_start);
    }

    pub(super) fn navigate_file_picker(&mut self, delta: i32) -> bool {
        if !self.has_file_query() {
            return false;
        }
        self.reconcile_file_picker();
        if self.file_picker_eligible() {
            let picker = &mut self.file_picker;
            picker
                .state
                .navigate(&mut picker.index, &mut picker.window_start, delta);
        }
        true
    }

    pub(super) fn autocomplete_file_picker(&mut self) -> InsertResult {
        self.reconcile_file_picker();
        if !self.file_picker_eligible() {
            return InsertResult::Inactive;
        }
        let Some(selection) = self.file_picker.selection(&self.composer) else {
            let picker = &mut self.file_picker;
            let status = picker.state.view(picker.index, picker.window_start).status;
            if status != Status::Loading
                && (matches!(status, Status::Unavailable | Status::Stale)
                    || picker.state.selection_missing)
            {
                picker.state.retry();
            }
            return InsertResult::Inactive;
        };
        self.file_picker.apply(&mut self.composer, &selection)
    }

    pub(super) fn submit_file_picker_on_enter(&mut self) -> Option<InsertResult> {
        self.reconcile_file_picker();
        if !self.file_picker_owns_surface() {
            return None;
        }
        match self.file_picker.selection(&self.composer) {
            Some(selection) => Some(self.file_picker.apply(&mut self.composer, &selection)),
            None if self.file_picker.state.accepted_empty() => None,
            None => Some(InsertResult::Inactive),
        }
    }

    pub(super) fn dismiss_file_picker(&mut self) -> bool {
        if !self.file_picker_owns_surface() {
            return false;
        }
        self.file_picker.state.invalidate();
        self.file_picker.suppressed = true;
        true
    }

    pub(super) fn file_picker_after_edit(&mut self) {
        if self.file_picker.suppressed
            && raw_file_query(self.composer.text(), self.composer.cursor()).is_none()
        {
            self.file_picker.suppressed = false;
        }
    }

    pub(super) fn reset_file_picker_episode(&mut self) {
        self.file_picker.state.invalidate();
        self.file_picker.suppressed = false;
    }
}

fn raw_file_query(text: &str, cursor: usize) -> Option<Query<'_>> {
    let trimmed = text.trim_start_matches([' ', '\t']);
    if PICKER_PREFIXES.iter().any(|prefix| {
        trimmed
            .get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    }) {
        return None;
    }
    let command = text.trim_start_matches(COMMAND_WHITESPACE);
    if command
        .strip_prefix(MCP_COMMAND)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
    {
        return None;
    }
    query_at(text, cursor)
}

#[cfg(test)]
mod tests;
