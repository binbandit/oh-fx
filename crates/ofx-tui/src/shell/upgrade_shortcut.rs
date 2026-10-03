use ofx_contract::{Notice, NoticeTone, UiCommand};

use super::Shell;
use crate::input::InputEvent;
use crate::render_engine::transcript_blocks::Entry;

const CTRL_G: u8 = 0x07;
const UPGRADE_TOPIC: &str = "upgrade";

pub(super) fn requests_upgrade(event: &InputEvent) -> bool {
    matches!(event, InputEvent::Raw(raw) if raw.byte == CTRL_G)
}

impl Shell<'_> {
    pub(super) fn apply_ready_upgrade(&mut self) {
        match self.upgrade_refusal() {
            Some(refusal) => self.push_entry(Entry::Notice(Notice::new(
                NoticeTone::Neutral,
                UPGRADE_TOPIC,
                refusal,
            ))),
            None => self.send(UiCommand::ApplyReadyUpgrade),
        }
    }

    fn upgrade_refusal(&self) -> Option<&'static str> {
        if self.question.is_some() {
            return Some("upgrade is unavailable while a question is open");
        }
        if self.approval.is_some() {
            return Some("upgrade is unavailable while an approval is open");
        }
        if self.working() || !self.outstanding.is_empty() {
            return Some("upgrade is unavailable until the response finishes");
        }
        if self.picker_active() {
            return Some("close the session picker before upgrading");
        }
        if !self.composer.is_empty() {
            return Some("submit or clear the current prompt before upgrading");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use ofx_contract::{
        ApprovalOrigin, ApprovalRequest, ApprovalScope, CallDescription, Concurrency, PathAccess,
        QuestionBatchEntry, QuestionOption, QuestionRequest, RequestId, SessionScope, ToolActivity,
        ToolCallId, ToolEffect, TurnId, TurnOutcome, UiEvent,
    };

    use super::super::test_shell::TestShell;
    use super::*;

    fn press_ctrl_g(test: &mut TestShell) -> String {
        test.screen();
        test.type_bytes(&[CTRL_G]);
        test.step();
        test.screen()
    }

    fn upgrade_requests(test: &TestShell) -> usize {
        test.sent()
            .iter()
            .filter(|command| **command == UiCommand::ApplyReadyUpgrade)
            .count()
    }

    fn working() -> TestShell {
        let mut test = TestShell::start();
        test.submit("fix the build");
        test.deliver(UiEvent::TurnStarted {
            turn_id: TurnId::new(1),
        });
        test
    }

    fn approval() -> UiEvent {
        UiEvent::ApprovalRequested {
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
                change: None,
                origin: ApprovalOrigin::ActiveSession,
            }),
        }
    }

    fn question() -> UiEvent {
        UiEvent::QuestionRequested {
            turn_id: TurnId::new(1),
            request: QuestionRequest {
                id: RequestId::new(5),
                entries: vec![QuestionBatchEntry {
                    question: "Continue?".to_owned(),
                    options: vec![QuestionOption {
                        label: "Alpha".to_owned(),
                        description: None,
                    }],
                }],
            },
        }
    }

    #[test]
    fn ctrl_g_asks_the_app_to_apply_a_ready_upgrade_without_touching_the_composer() {
        let mut test = TestShell::start();
        press_ctrl_g(&mut test);
        assert_eq!(test.sent(), [UiCommand::ApplyReadyUpgrade]);
        assert!(test.shell.composer.is_empty());
    }

    #[test]
    fn ctrl_g_waits_for_the_response_to_finish() {
        let mut test = working();
        let screen = press_ctrl_g(&mut test);
        assert!(
            screen.contains("upgrade is unavailable until the response finishes"),
            "{screen}"
        );
        assert_eq!(upgrade_requests(&test), 0);
        test.deliver(UiEvent::TurnFinished {
            turn_id: TurnId::new(1),
            outcome: TurnOutcome::Completed,
        });
        press_ctrl_g(&mut test);
        assert_eq!(upgrade_requests(&test), 1);
    }

    #[test]
    fn ctrl_g_leaves_an_open_question_or_approval_in_place() {
        for (event, refusal) in [
            (
                question(),
                "upgrade is unavailable while a question is open",
            ),
            (
                approval(),
                "upgrade is unavailable while an approval is open",
            ),
        ] {
            let mut test = working();
            test.deliver(event);
            let screen = press_ctrl_g(&mut test);
            assert!(screen.contains(refusal), "{screen}");
            assert_eq!(upgrade_requests(&test), 0);
            assert!(test.shell.question.is_some() || test.shell.approval.is_some());
        }
    }

    #[test]
    fn ctrl_g_asks_to_close_the_session_picker_first() {
        let mut test = TestShell::start();
        test.deliver(UiEvent::SessionPickerOpened {
            scope: SessionScope::CurrentWorkspace,
        });
        let screen = press_ctrl_g(&mut test);
        assert!(
            screen.contains("close the session picker before upgrading"),
            "{screen}"
        );
        assert_eq!(upgrade_requests(&test), 0);
    }

    #[test]
    fn ctrl_g_keeps_a_draft_and_asks_to_submit_or_clear_it() {
        let mut test = TestShell::start();
        test.type_bytes(b"draft");
        test.step();
        let screen = press_ctrl_g(&mut test);
        assert!(
            screen.contains("submit or clear the current prompt before upgrading"),
            "{screen}"
        );
        assert_eq!(test.shell.composer.text(), "draft");
        assert_eq!(upgrade_requests(&test), 0);
    }
}
