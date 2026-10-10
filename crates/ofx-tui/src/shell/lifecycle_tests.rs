use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ofx_contract::{
    ApprovalOrigin, ApprovalRequest, ApprovalScope, AttentionKind, CallDescription, Concurrency,
    HookRuntime, HookScope, PathAccess, QuestionBatchEntry, QuestionOption, QuestionRequest,
    RequestId, ToolActivity, ToolCallId, ToolEffect, TurnId, TurnOutcome, UiCommand, UiEvent,
};

use super::ShellOptions;
use super::test_shell::TestShell;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Report {
    Attention(HookScope, TurnId, AttentionKind),
}

#[derive(Clone, Default)]
struct Reports(Arc<Mutex<Vec<Report>>>);

impl Reports {
    fn observe(&self, options: &mut ShellOptions) {
        let mut hooks = HookRuntime::default();
        let attention = self.clone();
        hooks
            .register_attention_required("test.attention_required", move |input| {
                attention.0.lock().unwrap().push(Report::Attention(
                    input.invocation.scope,
                    input.invocation.turn_id,
                    input.kind,
                ));
            })
            .unwrap();
        options.hooks = hooks.freeze();
    }

    fn take(&self) -> Vec<Report> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

fn question(turn: u64, id: u64) -> UiEvent {
    UiEvent::QuestionRequested {
        turn_id: TurnId::new(turn),
        request: QuestionRequest {
            id: RequestId::new(id),
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

fn approval(turn: u64, id: u64) -> UiEvent {
    UiEvent::ApprovalRequested {
        turn_id: TurnId::new(turn),
        request: Box::new(ApprovalRequest {
            id: RequestId::new(id),
            tool_name: "read_file".to_owned(),
            call_id: ToolCallId::new(format!("read-{id}")),
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

fn attention(turn: u64, kind: AttentionKind) -> Report {
    Report::Attention(HookScope::Interactive, TurnId::new(turn), kind)
}

#[test]
fn attention_hooks_ignore_invisible_prompts_and_turn_boundaries() {
    let reports = Reports::default();
    let mut test = TestShell::start_with(|options| reports.observe(options));
    test.deliver(question(99, 99));
    test.deliver(approval(99, 99));
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(99),
    });
    test.deliver(UiEvent::TurnFinished {
        turn_id: TurnId::new(99),
        outcome: TurnOutcome::Completed,
    });
    assert!(reports.take().is_empty());
    test.submit("work");
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(1),
    });
    test.deliver(approval(1, 1));
    let _ = test.screen();
    let _ = test.screen();
    test.deliver(question(1, 1));
    test.deliver(UiEvent::TurnFinished {
        turn_id: TurnId::new(1),
        outcome: TurnOutcome::Interrupted,
    });
    assert_eq!(
        reports.take(),
        [
            attention(1, AttentionKind::Permission),
            attention(1, AttentionKind::Question),
        ]
    );
}

#[test]
fn attention_hooks_run_only_when_a_prompt_becomes_active() {
    let reports = Reports::default();
    let mut test = TestShell::start_with(|options| reports.observe(options));
    test.submit("work");
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(1),
    });
    assert!(reports.take().is_empty());
    test.deliver(approval(1, 1));
    test.deliver(approval(1, 2));
    test.deliver(question(1, 3));
    test.deliver(question(1, 4));
    assert_eq!(
        reports.take(),
        [
            attention(1, AttentionKind::Permission),
            attention(1, AttentionKind::Question),
        ]
    );
    test.type_bytes(b"\x03");
    test.step();
    assert!(test.sent().iter().any(|command| matches!(
        command,
        UiCommand::Approval { request_id, .. } if *request_id == RequestId::new(2)
    )));
    test.deliver(approval(1, 5));
    assert_eq!(reports.take(), [attention(1, AttentionKind::Permission)]);
}
