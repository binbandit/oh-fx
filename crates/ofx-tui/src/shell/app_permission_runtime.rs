use ofx_contract::{PermissionMode, UiCommand};

use super::Shell;

const YOLO_WARNING_VISIBLE_MS: i64 = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct YoloWarning {
    active: bool,
    remaining_visible_ms: i64,
    visible_since_ms: Option<i64>,
    announced: bool,
}

impl Default for YoloWarning {
    fn default() -> Self {
        Self {
            active: false,
            remaining_visible_ms: YOLO_WARNING_VISIBLE_MS,
            visible_since_ms: None,
            announced: false,
        }
    }
}

impl YoloWarning {
    pub(super) fn new(armed: bool) -> Self {
        let mut warning = Self::default();
        if armed {
            warning.arm();
        }
        warning
    }

    pub(super) fn active(&self) -> bool {
        self.active
    }

    pub(super) fn deadline_ms(&self) -> Option<i64> {
        self.visible_since_ms
            .filter(|_| self.active)
            .map(|since| since.saturating_add(self.remaining_visible_ms))
    }

    fn arm(&mut self) {
        *self = Self {
            active: true,
            ..Self::default()
        };
    }

    fn frame_committed(&mut self, now_ms: i64, included: bool) -> bool {
        if !self.active {
            return false;
        }
        if !included {
            self.pause(now_ms);
            return false;
        }
        self.visible_since_ms.get_or_insert(now_ms);
        !std::mem::replace(&mut self.announced, true)
    }

    fn tick(&mut self, now_ms: i64) -> bool {
        if !self.active {
            return false;
        }
        let Some(started) = self.visible_since_ms else {
            return false;
        };
        if (now_ms - started).max(0) < self.remaining_visible_ms {
            return false;
        }
        self.active = false;
        self.remaining_visible_ms = 0;
        self.visible_since_ms = None;
        true
    }

    fn pause(&mut self, now_ms: i64) {
        let Some(started) = self.visible_since_ms.take() else {
            return;
        };
        let elapsed = (now_ms - started).max(0);
        self.remaining_visible_ms = (self.remaining_visible_ms - elapsed).max(0);
        if self.remaining_visible_ms == 0 {
            self.active = false;
        }
    }
}

impl Shell<'_> {
    pub(super) fn permission_mode_changed(&mut self, mode: PermissionMode, warn: bool) {
        self.options.permission_mode = mode;
        self.yolo_warning = YoloWarning::new(warn);
    }

    pub(super) fn note_frame_committed(&mut self, now_ms: i64, warning_included: bool) {
        if self.yolo_warning.frame_committed(now_ms, warning_included) {
            self.send(UiCommand::FullAccessWarningShown);
        }
    }

    pub(super) fn expire_yolo_warning(&mut self, now_ms: i64) {
        if self.yolo_warning.tick(now_ms) {
            self.mark_dirty();
        }
    }
}

#[cfg(test)]
mod tests {
    use ofx_contract::{
        ApprovalOrigin, ApprovalRequest, ApprovalScope, CallDescription, Concurrency,
        FULL_ACCESS_WARNING, PathAccess, RequestId, ToolActivity, ToolCallId, ToolEffect, TurnId,
        UiEvent,
    };

    use super::*;
    use crate::shell::test_shell::TestShell;

    const SHIFT_TAB: &[u8] = b"\x1b[Z";

    fn changed(mode: PermissionMode, full_access_warning: bool) -> UiEvent {
        UiEvent::PermissionModeChanged {
            mode,
            full_access_warning,
        }
    }

    fn toggles(test: &TestShell) -> usize {
        test.sent()
            .iter()
            .filter(|command| **command == UiCommand::TogglePermissionMode)
            .count()
    }

    fn acknowledgments(test: &TestShell) -> usize {
        test.sent()
            .iter()
            .filter(|command| **command == UiCommand::FullAccessWarningShown)
            .count()
    }

    #[test]
    fn yolo_warning_counts_only_committed_visible_intervals() {
        let mut warning = YoloWarning::new(true);
        assert!(warning.frame_committed(100, true));
        assert!(!warning.frame_committed(1100, false));
        assert_eq!(warning.remaining_visible_ms, 3000);
        assert!(!warning.tick(10_000));
        assert!(!warning.frame_committed(12_000, true));
        assert_eq!(warning.deadline_ms(), Some(15_000));
        assert!(!warning.tick(14_999));
        assert!(warning.tick(15_000));
        assert!(!warning.active());
        assert_eq!(warning.deadline_ms(), None);
    }

    #[test]
    fn yolo_warning_follows_the_first_committed_warning_frame() {
        let mut warning = YoloWarning::new(true);
        assert!(!warning.frame_committed(100, false));
        assert!(warning.frame_committed(200, true));
        assert_eq!(warning.visible_since_ms, Some(200));
        assert!(!warning.frame_committed(300, true));
        assert!(!warning.tick(4199));
        assert!(warning.active());
        assert!(warning.tick(4200));
        assert!(!warning.active());
        let mut idle = YoloWarning::new(false);
        assert!(!idle.frame_committed(100, true));
        assert!(!idle.tick(10_000));
    }

    #[test]
    fn shift_tab_asks_the_controller_to_cycle_the_mode_and_the_footer_follows_its_answer() {
        let mut test = TestShell::start();
        assert!(test.screen().contains("auto · model-a"));
        test.type_bytes(SHIFT_TAB);
        test.step();
        assert_eq!(toggles(&test), 1);
        assert!(test.shell.composer.is_empty());
        assert!(test.screen().contains("auto · model-a"));
        test.deliver(changed(PermissionMode::Yolo, false));
        let screen = test.screen();
        assert!(screen.contains("full access · model-a"), "{screen}");
        assert!(!screen.contains(FULL_ACCESS_WARNING), "{screen}");
        test.deliver(changed(PermissionMode::Ask, false));
        assert!(test.screen().contains("ask · model-a"));
        test.type_bytes(b"draft\x1b[Z");
        test.step();
        assert_eq!(toggles(&test), 2);
        assert_eq!(test.shell.composer.text(), "draft");
    }

    #[test]
    fn shift_tab_leaves_the_mode_alone_while_an_approval_is_open() {
        let mut test = TestShell::start();
        test.submit("read it");
        test.deliver(UiEvent::TurnStarted {
            turn_id: TurnId::new(1),
        });
        test.deliver(UiEvent::ApprovalRequested {
            turn_id: TurnId::new(1),
            request: Box::new(ApprovalRequest {
                id: RequestId::new(4),
                tool_name: "read_file".to_owned(),
                call_id: ToolCallId::new("call-1"),
                description: CallDescription {
                    title: "Reading ../notes.txt".to_owned(),
                    label: None,
                    activity: ToolActivity::Read,
                    effect: ToolEffect::ReadOnly,
                    concurrency: Concurrency::Parallel,
                },
                tool_arguments_preview: "{}".to_owned(),
                tool_arguments_truncated: false,
                scope: ApprovalScope {
                    target: None,
                    access: PathAccess::WorkspaceOnly,
                    always: None,
                },
                command: None,
                file: None,
                origin: ApprovalOrigin::ActiveSession,
            }),
        });
        test.screen();
        test.type_bytes(SHIFT_TAB);
        test.step();
        assert_eq!(toggles(&test), 0);
        assert!(test.shell.approval.is_some());
    }

    #[test]
    fn switching_into_full_access_shows_the_warning_until_it_has_been_visible_for_four_seconds() {
        let mut test = TestShell::start();
        test.screen();
        test.deliver(changed(PermissionMode::Yolo, true));
        let screen = test.screen();
        assert!(screen.contains(FULL_ACCESS_WARNING), "{screen}");
        assert!(screen.contains("full access · model-a"), "{screen}");
        assert_eq!(acknowledgments(&test), 1);
        test.screen();
        assert_eq!(acknowledgments(&test), 1);
        let deadline = test.shell.yolo_warning.deadline_ms().unwrap();
        let now_ms = test.shell.now_ms();
        assert!((now_ms + 3_000..=now_ms + 4_000).contains(&deadline));
        assert!(test.shell.next_deadline_ms(now_ms) <= Some(deadline));
        test.advance(1_000);
        assert!(test.screen().contains(FULL_ACCESS_WARNING));
        assert!(test.shell.yolo_warning.active());
        test.advance(3_000);
        test.step();
        let screen = test.screen();
        assert!(!screen.contains(FULL_ACCESS_WARNING), "{screen}");
        assert!(screen.contains("full access · model-a"), "{screen}");
        assert_eq!(test.shell.yolo_warning.deadline_ms(), None);
    }

    #[test]
    fn a_hidden_warning_does_not_spend_its_visible_time_and_leaving_full_access_clears_it() {
        let mut test = TestShell::start();
        test.deliver(changed(PermissionMode::Yolo, true));
        test.screen();
        test.type_bytes(b"x\x1b");
        test.step();
        test.advance(40);
        test.draining(|shell| shell.flush_pending_input().unwrap());
        assert!(test.shell.gestures.escape_clear_armed());
        let screen = test.screen();
        assert!(screen.contains("esc again to clear"), "{screen}");
        assert!(!screen.contains(FULL_ACCESS_WARNING), "{screen}");
        assert_eq!(test.shell.yolo_warning.deadline_ms(), None);
        assert!(test.shell.yolo_warning.active());
        test.deliver(changed(PermissionMode::Ask, false));
        let screen = test.screen();
        assert!(!test.shell.yolo_warning.active());
        assert!(!screen.contains("Full access"), "{screen}");
        assert!(screen.contains("ask · model-a"), "{screen}");
    }

    #[test]
    fn a_session_that_starts_in_unacknowledged_full_access_warns_on_its_first_frame() {
        let mut test = TestShell::start_with(|options| {
            options.permission_mode = PermissionMode::Yolo;
            options.full_access_warning = true;
        });
        let screen = test.screen();
        assert!(screen.contains(FULL_ACCESS_WARNING), "{screen}");
        assert_eq!(test.sent(), [UiCommand::FullAccessWarningShown]);
    }
}
