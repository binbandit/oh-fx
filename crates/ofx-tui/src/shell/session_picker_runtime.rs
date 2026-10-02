use std::time::{SystemTime, UNIX_EPOCH};

use ofx_contract::{
    HistoryEntry, ResumeRefusal, SessionCursor, SessionPage, SessionRow, SessionScope, UiCommand,
};
use ofx_text::contains_ignore_case;

use super::{FreshScreen, Shell};
use crate::footer::resume_menu_presentation::{
    LoadState, MAX_INLINE_ROWS, SessionMenuView, menu_frame,
};
use crate::row_text::Row;
use crate::terminal::Layout;
use crate::theme::Theme;
use crate::transcript::history_replay::replayed_entries;

const DEFAULT_PAGE_LIMIT: usize = 10;
const PAGE_CHROME_ROWS: usize = 7;
const QUERY_TRIM: &[char] = &[' ', '\t', '\r', '\n'];
const FALLBACK_TITLE: &str = "Untitled session";

pub(super) struct SessionPicker {
    scope: SessionScope,
    load: LoadState,
    rows: Vec<SessionRow>,
    has_more: bool,
    loading_more: bool,
    selected: usize,
    window_start: usize,
    refusal: Option<ResumeRefusal>,
    query: String,
}

impl SessionPicker {
    fn new(scope: SessionScope, query: String) -> Self {
        Self {
            scope,
            load: LoadState::Loading,
            rows: Vec::new(),
            has_more: false,
            loading_more: false,
            selected: 0,
            window_start: 0,
            refusal: None,
            query,
        }
    }

    fn filtered(&self) -> Vec<&SessionRow> {
        let query = self.query.trim_matches(QUERY_TRIM);
        self.rows
            .iter()
            .filter(|row| {
                query.is_empty()
                    || contains_ignore_case(row.title.as_deref().unwrap_or(FALLBACK_TITLE), query)
                    || contains_ignore_case(&row.workspace_root, query)
            })
            .collect()
    }

    fn selected_id(&self) -> Option<String> {
        if self.load != LoadState::Ready {
            return None;
        }
        self.filtered().get(self.selected).map(|row| row.id.clone())
    }

    fn load_more_selected(&self) -> bool {
        self.load == LoadState::Ready && self.has_more && self.selected == self.filtered().len()
    }

    fn move_selection(&mut self, delta: isize) {
        if self.load != LoadState::Ready {
            return;
        }
        self.refusal = None;
        let count = self.filtered().len() + usize::from(self.has_more);
        if count == 0 {
            return;
        }
        self.selected = (self.selected % count)
            .saturating_add_signed(delta)
            .min(count - 1);
    }

    fn wants_prefetch(&self) -> bool {
        self.load == LoadState::Ready
            && self.has_more
            && !self.loading_more
            && !self.rows.is_empty()
            && self.selected + 1 >= self.filtered().len()
    }

    fn set_query(&mut self, query: &str) {
        if self.query == query {
            return;
        }
        query.clone_into(&mut self.query);
        self.selected = 0;
        self.window_start = 0;
        self.refusal = None;
    }

    pub(super) fn menu_rows(
        &mut self,
        theme: &Theme,
        layout: Layout,
        composer_rows: usize,
    ) -> Vec<Row> {
        let budget = MAX_INLINE_ROWS
            .min(usize::from(layout.rows).saturating_sub(composer_rows + 1))
            .max(1);
        let view = SessionMenuView {
            scope: self.scope,
            load: self.load,
            rows: self.filtered(),
            has_more: self.has_more,
            loading_more: self.loading_more,
            selected: self.selected,
            window_start: self.window_start,
            refusal: self.refusal,
            now_ms: wall_clock_ms(),
        };
        let frame = menu_frame(&view, theme, usize::from(layout.cols), budget);
        self.window_start = frame.window_start;
        frame.rows
    }

    fn cursor(&self) -> Option<SessionCursor> {
        self.rows.last().map(|row| SessionCursor {
            updated_at_ms: row.updated_at_ms,
            id: row.id.clone(),
        })
    }
}

impl Shell<'_> {
    pub(super) fn session_picker_opened(&mut self, scope: SessionScope) {
        let query = self.composer.text().to_owned();
        self.picker = Some(SessionPicker::new(scope, query));
        self.request_first_page(scope);
    }

    fn request_first_page(&mut self, scope: SessionScope) {
        let limit = self.page_limit();
        self.send(UiCommand::ListSessions {
            scope,
            after: None,
            limit,
        });
    }

    fn page_limit(&self) -> usize {
        usize::from(self.layout.rows)
            .saturating_sub(PAGE_CHROME_ROWS)
            .max(DEFAULT_PAGE_LIMIT)
    }

    pub(super) fn sessions_listed(&mut self, page: SessionPage) {
        let Some(picker) = self
            .picker
            .as_mut()
            .filter(|picker| picker.scope == page.scope)
        else {
            return;
        };
        if page.after.is_none() {
            picker.rows = page.rows;
            picker.load = LoadState::Ready;
            picker.has_more = page.has_more;
            picker.loading_more = false;
            picker.selected = 0;
            picker.window_start = 0;
        } else if picker.loading_more && page.after == picker.cursor() {
            picker.rows.extend(page.rows);
            picker.has_more = page.has_more;
            picker.loading_more = false;
        }
    }

    pub(super) fn sessions_unavailable(&mut self, scope: SessionScope) {
        let Some(picker) = self.picker.as_mut().filter(|picker| picker.scope == scope) else {
            return;
        };
        if picker.loading_more {
            picker.loading_more = false;
        } else {
            picker.load = LoadState::Failed;
        }
    }

    pub(super) fn session_resume_failed(&mut self, id: &str, refusal: ResumeRefusal) {
        if let Some(picker) = &mut self.picker
            && picker.selected_id().as_deref() == Some(id)
        {
            picker.refusal = Some(refusal);
        }
    }

    pub(super) fn session_resumed(&mut self, history: Vec<HistoryEntry>) {
        self.picker = None;
        self.composer.clear();
        self.restart_transcript(FreshScreen::Erase, replayed_entries(history));
    }

    pub(super) fn picker_active(&self) -> bool {
        self.picker.is_some()
    }

    pub(super) fn submit_picker_selection(&mut self) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        if picker.load_more_selected() {
            self.load_more_sessions();
            return;
        }
        if let Some(id) = picker.selected_id() {
            self.send(UiCommand::ResumeSession { id });
        }
    }

    fn load_more_sessions(&mut self) {
        let limit = self.page_limit();
        let Some(picker) = self.picker.as_mut().filter(|picker| !picker.loading_more) else {
            return;
        };
        picker.loading_more = true;
        let command = UiCommand::ListSessions {
            scope: picker.scope,
            after: picker.cursor(),
            limit,
        };
        self.send(command);
    }

    pub(super) fn move_picker(&mut self, delta: isize) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        picker.move_selection(delta);
        if picker.wants_prefetch() {
            self.load_more_sessions();
        }
    }

    pub(super) fn toggle_picker_scope(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let scope = picker.scope.toggled();
        self.picker = Some(SessionPicker::new(scope, picker.query));
        self.request_first_page(scope);
    }

    pub(super) fn close_picker(&mut self) {
        if self.picker.take().is_some() {
            self.composer.clear();
            self.send(UiCommand::CloseSessionPicker);
        }
    }

    pub(super) fn sync_picker_query(&mut self) {
        let text = self.composer.text().to_owned();
        if let Some(picker) = &mut self.picker {
            picker.set_query(&text);
        }
    }
}

fn wall_clock_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}
