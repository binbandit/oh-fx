use ofx_contract::{ApprovalDecision, ApprovalRequest, GatedAction, RequestId};

use super::*;

#[derive(Default)]
struct RememberingGate {
    remembered: Mutex<Vec<String>>,
}

impl PermissionGate for RememberingGate {
    fn admit(&self, call: &ToolCall) -> Admission {
        ArgumentGate.admit(call)
    }

    fn admit_file_mutation(&self, mutation: &FileMutation) -> Admission {
        ArgumentGate.admit_file_mutation(mutation)
    }

    fn admit_command(&self, request: &CommandRequest) -> Admission {
        ArgumentGate.admit_command(request)
    }

    fn applicable_target(&self, call: &ToolCall) -> Option<ApplicableTarget> {
        ArgumentGate.applicable_target(call)
    }

    fn remember_approval(&self, action: GatedAction<'_>) {
        let remembered = match action {
            GatedAction::Call(call) => format!("call {}", call.id.as_str()),
            GatedAction::FileMutation(mutation) => {
                format!("file {}", mutation.target.display())
            }
            GatedAction::Command(request) => format!("command {request:?}"),
        };
        self.remembered.lock().unwrap().push(remembered);
    }

    fn approved_access(&self, _action: GatedAction<'_>) -> PathAccess {
        PathAccess::Within(PathBuf::from("/approved"))
    }
}

async fn run_approving(
    provider: Arc<FakeProvider>,
    gate: Arc<RememberingGate>,
    decide: impl Fn(&ApprovalRequest) -> Option<ApprovalDecision> + Sync,
) -> (TurnReport, Vec<UiEvent>, Approvals) {
    let approvals = Approvals::default();
    let mut agent = Agent::new(
        provider,
        vec![echo_tool()],
        Arc::new(FixedContext),
        gate,
        config(),
    )
    .with_approvals(approvals.clone());
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    let report = agent
        .run_turn(
            "go",
            &mut |event| {
                if let UiEvent::ApprovalRequested { request, .. } = &event {
                    match decide(request) {
                        Some(decision) => assert!(approvals.resolve(request.id, decision)),
                        None => cancel.cancel(),
                    }
                }
                events.push(event);
            },
            &cancel,
        )
        .await;
    (report, events, approvals)
}

fn approval_requests(events: &[UiEvent]) -> Vec<ApprovalRequest> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ApprovalRequested { request, .. } => Some(request.clone()),
            _ => None,
        })
        .collect()
}

fn started_titles(events: &[UiEvent]) -> Vec<(&str, &str)> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ToolStarted {
                call_id,
                description,
                ..
            } => Some((call_id.as_str(), description.title.as_str())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn approved_calls_run_with_the_access_their_approval_grants_and_always_is_remembered() {
    for (decision, remembered) in [
        (ApprovalDecision::Once, Vec::<String>::new()),
        (ApprovalDecision::Always, vec!["call call-2".to_owned()]),
    ] {
        let provider = FakeProvider::new(vec![
            tool_reply(&[
                ("call-1", r#"{"access":"workspace"}"#),
                ("call-2", r#"{"access":"outside"}"#),
            ]),
            text_reply("done"),
        ]);
        let gate = Arc::new(RememberingGate::default());
        let (report, events, _) =
            run_approving(Arc::clone(&provider), Arc::clone(&gate), |_| Some(decision)).await;
        assert_eq!(report.outcome, TurnOutcome::Completed, "{decision:?}");
        assert_eq!(
            approval_requests(&events),
            [ApprovalRequest {
                id: RequestId::new(1),
                tool_name: "echo".to_owned(),
                title: r#"Echoing {"access":"outside"}"#.to_owned(),
            }]
        );
        assert_eq!(
            provider.requests()[1].messages[2..],
            [
                tool_message("call-1", "WorkspaceOnly", ToolResultStatus::Success),
                tool_message(
                    "call-2",
                    r#"Within("/approved")"#,
                    ToolResultStatus::Success
                ),
            ]
        );
        assert_eq!(*gate.remembered.lock().unwrap(), remembered);
    }
}

#[tokio::test]
async fn denied_calls_report_the_denial_to_the_model_and_the_turn_continues() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"path":"outside"}"#)]),
        text_reply("I could not read it."),
    ]);
    let gate = Arc::new(RememberingGate::default());
    let (report, events, _) = run_approving(Arc::clone(&provider), Arc::clone(&gate), |_| {
        Some(ApprovalDecision::Deny)
    })
    .await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "I could not read it.");
    assert_eq!(dispatch_order(&events), ["start call-1", "finish call-1"]);
    assert_eq!(finished(&events), [("call-1", ToolResultStatus::Failure)]);
    assert_eq!(
        provider.requests()[1].messages[2..],
        [tool_message(
            "call-1",
            &tool_permission_denied_json("echo"),
            ToolResultStatus::Failure
        )]
    );
    assert!(gate.remembered.lock().unwrap().is_empty());
}

#[tokio::test]
async fn file_changes_and_commands_ask_with_their_target_and_remember_their_action() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"changes":1,"serial":true}"#),
            ("call-2", r#"{"stop":1,"serial":true}"#),
            ("call-3", r#"{"changes":2,"serial":true}"#),
        ]),
        text_reply("done"),
    ]);
    let gate = Arc::new(RememberingGate::default());
    let (report, events, _) = run_approving(Arc::clone(&provider), Arc::clone(&gate), |request| {
        Some(if request.title.contains(r#""changes":2"#) {
            ApprovalDecision::Deny
        } else {
            ApprovalDecision::Always
        })
    })
    .await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let titles: Vec<String> = approval_requests(&events)
        .into_iter()
        .map(|request| request.title)
        .collect();
    assert_eq!(
        titles,
        [
            r#"Echoing {"changes":1,"serial":true}"#,
            r#"Echoing {"stop":1,"serial":true}"#,
            r#"Echoing {"changes":2,"serial":true}"#,
        ]
    );
    assert_eq!(
        started_titles(&events),
        [
            ("call-1", r#"Echoing {"changes":1,"serial":true}"#),
            ("call-2", r#"Echoing {"stop":1,"serial":true}"#),
            ("call-3", "Echoing file"),
        ]
    );
    assert_eq!(
        *gate.remembered.lock().unwrap(),
        ["file /workspace/note.txt", "command Stop"]
    );
    assert_eq!(
        finished(&events),
        [
            ("call-1", ToolResultStatus::Success),
            ("call-2", ToolResultStatus::Success),
            ("call-3", ToolResultStatus::Failure),
        ]
    );
}

#[tokio::test]
async fn cancelling_while_an_approval_is_pending_interrupts_without_running_the_call() {
    let provider = FakeProvider::new(vec![tool_reply(&[
        ("call-1", r#"{"text":"before"}"#),
        ("call-2", r#"{"path":"outside"}"#),
    ])]);
    let (report, events, approvals) =
        run_approving(provider, Arc::new(RememberingGate::default()), |_| None).await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(dispatch_order(&events), ["start call-1", "finish call-1"]);
    let request = approval_requests(&events).remove(0);
    assert!(!approvals.resolve(request.id, ApprovalDecision::Once));
}
