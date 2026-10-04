use std::time::{SystemTime, UNIX_EPOCH};

use ofx_contract::{
    HistoryEntry, Notice, NoticeTone, ResumeRefusal, SessionCursor, SessionPage, SessionRow,
    SessionScope, UiCommand,
};
use ofx_text::contains_ignore_case;

use super::{FreshScreen, Shell};
use crate::footer::resume_menu_presentation::{
    FALLBACK_TITLE, LoadState, MAX_INLINE_ROWS, SessionMenuView, menu_frame,
};
use crate::render_engine::transcript_blocks::Entry;
use crate::row_text::Row;
use crate::terminal::Layout;
use crate::theme::Theme;
use crate::transcript::history_replay::replayed_entries;

const DEFAULT_PAGE_LIMIT: usize = 10;
const PAGE_CHROME_ROWS: usize = 7;
const QUERY_TRIM: &[char] = &[' ', '\t', '\r', '\n'];
const DRAFT_BLOCKS_SWITCH: &str = "submit or clear the draft before switching sessions";

pub(super) struct SessionPicker {
    scope: SessionScope,
    load: LoadState,
    rows: Vec<SessionRow>,
    has_more: bool,
    loading_more: bool,
    selected: usize,
    window_start: usize,
    refusal: Option<ResumeRefusal>,
    accepting: Option<String>,
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
            accepting: None,
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

    fn menu_rows(&mut self, theme: &Theme, layout: Layout, composer_rows: usize) -> Vec<Row> {
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
        if self.accepting_session()
            || self.model_menu.is_some()
            || self.model_draft.is_some()
            || self.skills_menu.is_some()
        {
            return;
        }
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
        let Some(picker) = &mut self.picker else {
            return;
        };
        if picker.accepting.as_deref() == Some(id) {
            picker.accepting = None;
        }
        if picker.selected_id().as_deref() == Some(id) {
            picker.refusal = Some(refusal);
        }
    }

    pub(super) fn session_resumed(&mut self, history: Vec<HistoryEntry>) {
        self.picker = None;
        self.kept_recovery = None;
        self.statusline.conversation_cleared();
        self.composer.clear();
        self.restart_transcript(FreshScreen::Erase, replayed_entries(history));
    }

    pub(super) fn open_all_sessions(&mut self) {
        if self.skills_menu_visible() {
            return;
        }
        if self.composer.is_empty() {
            self.send(UiCommand::OpenSessions {
                scope: SessionScope::AllWorkspaces,
            });
            return;
        }
        self.push_entry(Entry::Notice(Notice::new(
            NoticeTone::Neutral,
            "session",
            DRAFT_BLOCKS_SWITCH,
        )));
    }

    pub(super) fn picker_active(&self) -> bool {
        self.picker.is_some()
    }

    pub(super) fn footer_menu(
        &mut self,
        composer_rows: usize,
        menu: Vec<Row>,
        hint: Row,
    ) -> (Vec<Row>, Option<Row>) {
        let sessions = match (&mut self.picker, &self.approval) {
            (Some(sessions), None) => sessions.menu_rows(&self.theme, self.layout, composer_rows),
            _ => Vec::new(),
        };
        if sessions.is_empty() {
            return (menu, Some(hint));
        }
        let mut band = vec![Row::new()];
        band.extend(sessions);
        (band, None)
    }

    pub(super) fn submit_picker_selection(&mut self) {
        let Some(picker) = self
            .picker
            .as_mut()
            .filter(|picker| picker.accepting.is_none())
        else {
            return;
        };
        if picker.load_more_selected() {
            self.load_more_sessions();
            return;
        }
        if let Some(id) = picker.selected_id() {
            picker.accepting = Some(id.clone());
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
        if self.accepting_session() {
            return;
        }
        let Some(picker) = self.picker.take() else {
            return;
        };
        let scope = picker.scope.toggled();
        self.picker = Some(SessionPicker::new(scope, picker.query));
        self.request_first_page(scope);
    }

    pub(super) fn close_picker(&mut self) {
        if self.accepting_session() {
            return;
        }
        if self.picker.take().is_some() {
            self.composer.clear();
            self.send(UiCommand::CloseSessionPicker);
        }
    }

    fn accepting_session(&self) -> bool {
        self.picker
            .as_ref()
            .is_some_and(|picker| picker.accepting.is_some())
    }

    pub(super) fn sync_picker_query(&mut self) {
        if let Some(picker) = &mut self.picker {
            picker.set_query(self.composer.text());
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

#[cfg(test)]
mod tests {
    use ofx_contract::{
        FULL_ACCESS_WARNING, HistoryEntry, PermissionMode, ResumeRefusal, SessionCursor,
        SessionPage, SessionRow, SessionScope, UiCommand, UiEvent,
    };

    use super::super::Opening;
    use super::super::test_shell::TestShell;

    fn row(id: &str, title: &str, updated_at_ms: i64) -> SessionRow {
        SessionRow {
            id: id.to_owned(),
            title: Some(title.to_owned()),
            workspace_root: "/work/proj".to_owned(),
            updated_at_ms,
            turns: 2,
        }
    }

    fn list(scope: SessionScope, after: Option<SessionCursor>) -> UiCommand {
        UiCommand::ListSessions {
            scope,
            after,
            limit: 17,
        }
    }

    fn opened(test: &mut TestShell) {
        test.deliver(UiEvent::SessionPickerOpened {
            scope: SessionScope::CurrentWorkspace,
        });
        test.deliver(UiEvent::SessionsListed {
            page: SessionPage {
                scope: SessionScope::CurrentWorkspace,
                after: None,
                rows: vec![row("a", "alpha", 3), row("b", "beta", 2)],
                has_more: true,
            },
        });
    }

    fn keys(test: &mut TestShell, bytes: &[u8]) {
        test.type_bytes(bytes);
        test.step();
    }

    #[test]
    fn a_shell_opened_to_pick_owns_the_keys_before_the_first_one_is_read() {
        let mut test = TestShell::start_with(|options| options.opening = Opening::SessionPicker);
        assert_eq!(test.sent(), [list(SessionScope::CurrentWorkspace, None)]);
        let screen = test.screen();
        assert!(screen.contains("Loading sessions…"), "{screen}");
        assert!(!screen.contains("Run /help"), "{screen}");
        keys(&mut test, b"early\r");
        assert_eq!(test.sent(), [list(SessionScope::CurrentWorkspace, None)]);
        assert!(test.screen().contains("┃ early"));
    }

    #[test]
    fn super_r_asks_for_every_workspace_once_the_draft_is_clear() {
        let mut test = TestShell::start();
        keys(&mut test, b"draft\x1b[114;9u");
        assert!(test.sent().is_empty());
        let screen = test.screen();
        assert!(
            screen.contains("* session: submit or clear the draft before switching sessions"),
            "{screen}"
        );
        keys(&mut test, b"\x7f\x7f\x7f\x7f\x7f\x1b[114;9u");
        assert_eq!(
            test.sent(),
            [UiCommand::OpenSessions {
                scope: SessionScope::AllWorkspaces
            }]
        );
    }

    #[test]
    fn the_full_access_warning_waits_until_the_picker_gives_back_the_status_line() {
        let mut test = TestShell::start_with(|options| {
            options.opening = Opening::SessionPicker;
            options.permission_mode = PermissionMode::Yolo;
            options.full_access_warning = true;
        });
        let screen = test.screen();
        assert!(!screen.contains(FULL_ACCESS_WARNING), "{screen}");
        assert!(!test.sent().contains(&UiCommand::FullAccessWarningShown));
        keys(&mut test, b"\x1b");
        test.advance(1_000);
        test.draining(super::super::Shell::flush_pending_input)
            .unwrap();
        let screen = test.screen();
        assert!(screen.contains(FULL_ACCESS_WARNING), "{screen}");
        assert_eq!(test.sent().last(), Some(&UiCommand::FullAccessWarningShown));
    }

    #[test]
    fn the_picker_lists_pages_and_moves_without_wrapping() {
        let mut test = TestShell::start();
        test.deliver(UiEvent::SessionPickerOpened {
            scope: SessionScope::CurrentWorkspace,
        });
        assert_eq!(test.sent(), [list(SessionScope::CurrentWorkspace, None)]);
        assert!(test.screen().contains("Loading sessions…"));
        opened(&mut test);
        let screen = test.screen();
        assert!(
            screen.contains("Sessions 2  [Current workspace]  All workspaces"),
            "{screen}"
        );
        assert!(screen.contains("  alpha    proj · "), "{screen}");
        assert!(screen.contains("  ↓ Load more"), "{screen}");
        assert!(!screen.contains("auto · model-a"), "{screen}");
        keys(&mut test, b"\x1b[A\x1b[B");
        let cursor = SessionCursor {
            updated_at_ms: 2,
            id: "b".to_owned(),
        };
        assert_eq!(
            test.sent()[2..],
            [list(SessionScope::CurrentWorkspace, Some(cursor.clone()))]
        );
        assert!(test.screen().contains("↓ Loading more…"));
        test.deliver(UiEvent::SessionsListed {
            page: SessionPage {
                scope: SessionScope::CurrentWorkspace,
                after: Some(cursor),
                rows: vec![row("c", "gamma", 1)],
                has_more: false,
            },
        });
        keys(&mut test, b"\x1b[B\x1b[B\x1b[B\r");
        assert_eq!(
            test.sent().last(),
            Some(&UiCommand::ResumeSession { id: "c".to_owned() })
        );
        assert!(test.screen().contains("Sessions 3"));
    }

    #[test]
    fn a_refused_choice_stays_in_the_picker_and_escape_closes_it() {
        let mut test = TestShell::start();
        opened(&mut test);
        keys(&mut test, b"\r");
        assert_eq!(
            test.sent().last(),
            Some(&UiCommand::ResumeSession { id: "a".to_owned() })
        );
        test.deliver(UiEvent::SessionResumeFailed {
            id: "a".to_owned(),
            refusal: ResumeRefusal::OpenElsewhere,
        });
        let screen = test.screen();
        assert!(
            screen.contains("  This session is open in another oh-fx."),
            "{screen}"
        );
        keys(&mut test, b"\x1b[B");
        assert!(!test.screen().contains("open in another oh-fx"));
        keys(&mut test, b"\x1b[Z");
        assert_eq!(
            test.sent().last(),
            Some(&list(SessionScope::AllWorkspaces, None))
        );
        let screen = test.screen();
        assert!(
            screen.contains("Sessions 0  Current workspace  [All workspaces]"),
            "{screen}"
        );
        test.deliver(UiEvent::SessionsListed {
            page: SessionPage {
                scope: SessionScope::CurrentWorkspace,
                after: None,
                rows: vec![row("stale", "stale page", 9)],
                has_more: false,
            },
        });
        assert!(!test.screen().contains("stale page"));
        keys(&mut test, b"draft");
        keys(&mut test, b"\x1b");
        test.advance(1_000);
        test.draining(super::super::Shell::flush_pending_input)
            .unwrap();
        assert_eq!(test.sent().last(), Some(&UiCommand::CloseSessionPicker));
        assert!(test.shell.composer.is_empty());
        let screen = test.screen();
        assert!(screen.contains("auto · model-a"), "{screen}");
    }

    fn escape(test: &mut TestShell) {
        keys(test, b"\x1b");
        test.advance(1_000);
        test.draining(super::super::Shell::flush_pending_input)
            .unwrap();
    }

    #[test]
    fn a_chosen_session_holds_the_picker_until_its_resume_settles() {
        let mut test = TestShell::start();
        opened(&mut test);
        keys(&mut test, b"\r");
        assert_eq!(
            test.sent().last(),
            Some(&UiCommand::ResumeSession { id: "a".to_owned() })
        );
        let sent = test.sent().len();
        escape(&mut test);
        keys(&mut test, b"\x1b[Z\r");
        keys(&mut test, b"for the old session\r");
        assert_eq!(test.sent().len(), sent, "{:?}", test.sent());
        let screen = test.screen();
        assert!(
            screen.contains("Sessions 0  [Current workspace]  All workspaces"),
            "{screen}"
        );
        test.deliver(UiEvent::SessionResumeFailed {
            id: "a".to_owned(),
            refusal: ResumeRefusal::Unavailable,
        });
        escape(&mut test);
        assert_eq!(test.sent().last(), Some(&UiCommand::CloseSessionPicker));
        assert!(
            !test
                .sent()
                .iter()
                .any(|command| matches!(command, UiCommand::Submit { .. })),
            "{:?}",
            test.sent()
        );
        assert!(test.shell.composer.is_empty());
    }

    #[test]
    fn an_open_queued_before_the_choice_leaves_the_pending_resume_in_place() {
        let mut test = TestShell::start();
        opened(&mut test);
        keys(&mut test, b"\x1b[114;9u\r");
        assert_eq!(
            test.sent()[1..],
            [
                UiCommand::OpenSessions {
                    scope: SessionScope::AllWorkspaces
                },
                UiCommand::ResumeSession { id: "a".to_owned() }
            ]
        );
        test.deliver(UiEvent::SessionPickerOpened {
            scope: SessionScope::AllWorkspaces,
        });
        let sent = test.sent().len();
        escape(&mut test);
        keys(&mut test, b"for the old session\r");
        assert_eq!(test.sent().len(), sent, "{:?}", test.sent());
        assert!(
            test.screen()
                .contains("Sessions 0  [Current workspace]  All workspaces")
        );
        test.deliver(UiEvent::SessionResumeFailed {
            id: "a".to_owned(),
            refusal: ResumeRefusal::Unavailable,
        });
        escape(&mut test);
        assert_eq!(test.sent().last(), Some(&UiCommand::CloseSessionPicker));
        assert!(
            !test
                .sent()
                .iter()
                .any(|command| matches!(command, UiCommand::Submit { .. })),
            "{:?}",
            test.sent()
        );
    }

    #[test]
    fn typed_text_filters_the_loaded_sessions() {
        let mut test = TestShell::start();
        opened(&mut test);
        keys(&mut test, b"BET");
        let screen = test.screen();
        assert!(screen.contains("Sessions 1"), "{screen}");
        assert!(screen.contains("┃ BET"), "{screen}");
        keys(&mut test, b"\r");
        assert_eq!(
            test.sent().last(),
            Some(&UiCommand::ResumeSession { id: "b".to_owned() })
        );
        keys(&mut test, b"x");
        let screen = test.screen();
        assert!(screen.contains("Sessions 0"), "{screen}");
        assert!(screen.contains("  ↓ Load more"), "{screen}");
    }

    #[test]
    fn a_resumed_session_replaces_the_screen_and_closes_the_picker() {
        let mut test = TestShell::start();
        test.submit("old prompt");
        opened(&mut test);
        test.deliver(UiEvent::SessionResumed {
            history: vec![
                HistoryEntry::User("alpha".to_owned()),
                HistoryEntry::Assistant("Reply.".to_owned()),
            ],
        });
        let screen = test.screen();
        assert!(!screen.contains("old prompt"), "{screen}");
        assert!(!screen.contains("Sessions"), "{screen}");
        assert!(screen.contains("┃ alpha\n\n  Reply."), "{screen}");
        assert!(screen.contains("auto · model-a"), "{screen}");
    }
}
