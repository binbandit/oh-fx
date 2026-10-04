use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ofx_contract::{
    ApprovalOrigin, ApprovalRequest, ApprovalScope, CallDescription, Concurrency, PathAccess,
    QuestionBatchEntry, QuestionOption, QuestionRequest, RequestId, ToolActivity, ToolCallId,
    ToolEffect, TurnId, TurnOutcome, UiEvent,
};

use super::test_shell::TestShell;
use crate::host::{ForegroundLifecycle, ForegroundState};

type Report = (String, Option<Vec<u8>>);

#[derive(Clone, Default)]
struct Reports(Arc<Mutex<Vec<Report>>>);

impl ForegroundLifecycle for Reports {
    fn shutdown(&self) {}

    fn report(&self, state: ForegroundState, status: Option<&[u8]>) {
        let state = match state {
            ForegroundState::Idle => "idle",
            ForegroundState::Working => "working",
            ForegroundState::Blocked => "blocked",
        };
        self.0
            .lock()
            .unwrap()
            .push((state.to_owned(), status.map(<[u8]>::to_vec)));
    }
}

fn question(turn: u64) -> UiEvent {
    UiEvent::QuestionRequested {
        turn_id: TurnId::new(turn),
        request: QuestionRequest {
            id: RequestId::new(turn),
            entries: vec![QuestionBatchEntry {
                question: "Continue?".to_owned(),
                options: vec![QuestionOption {
                    label: "Yes".to_owned(),
                    description: None,
                }],
            }],
        },
    }
}

fn approval(turn: u64) -> UiEvent {
    UiEvent::ApprovalRequested {
        turn_id: TurnId::new(turn),
        request: Box::new(ApprovalRequest {
            id: RequestId::new(turn),
            tool_name: "read_file".to_owned(),
            call_id: ToolCallId::new("read"),
            description: CallDescription {
                title: "Reading notes".to_owned(),
                label: None,
                activity: ToolActivity::Read,
                effect: ToolEffect::ReadOnly,
                concurrency: Concurrency::Serial,
            },
            tool_arguments_preview: "{}".to_owned(),
            tool_arguments_truncated: false,
            scope: ApprovalScope {
                target: Some(PathBuf::from("/notes")),
                access: PathAccess::Within(PathBuf::from("/")),
                always: None,
            },
            command: None,
            file: None,
            origin: ApprovalOrigin::ActiveSession,
            change: None,
        }),
    }
}

#[test]
fn foreground_observer_ignores_invisible_questions_and_unmatched_turns() {
    let reports = Reports::default();
    let copy = reports.clone();
    let mut test = TestShell::start_with(|options| options.lifecycle = Some(Box::new(copy)));
    test.deliver(question(99));
    test.deliver(approval(99));
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(99),
    });
    test.deliver(UiEvent::TurnFinished {
        turn_id: TurnId::new(99),
        outcome: TurnOutcome::Completed,
    });
    assert!(reports.0.lock().unwrap().is_empty());
    test.submit("work");
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(1),
    });
    test.deliver(approval(1));
    let _ = test.screen();
    let _ = test.screen();
    test.deliver(question(1));
    test.deliver(UiEvent::TurnFinished {
        turn_id: TurnId::new(1),
        outcome: TurnOutcome::Interrupted,
    });
    assert_eq!(
        *reports.0.lock().unwrap(),
        [
            ("working".to_owned(), None),
            ("blocked".to_owned(), Some(b"permission".to_vec())),
            ("blocked".to_owned(), Some(b"question".to_vec())),
            ("idle".to_owned(), None)
        ]
    );
}
