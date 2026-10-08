use ofx_contract::{
    ApprovalAnswer, ApprovalDecision, ApprovalOrigin, ApprovalRequest, ApprovalScope, GatedAction,
    ProposedFileChange, RequestId, SessionGrant,
};

use super::turn_log::{Logged, MemoryLog, logged};
use super::*;

#[derive(Default)]
struct RememberingGate {
    scopes: AtomicUsize,
    remembered: Mutex<Vec<String>>,
}

fn approved_tree(scope: usize) -> ApprovalScope {
    let tree = PathBuf::from(format!("/approved/{scope}"));
    ApprovalScope {
        target: Some(tree.join("target")),
        access: PathAccess::Within(tree.clone()),
        always: Some(SessionGrant::ReadsUnder(tree)),
    }
}

impl PermissionGate for RememberingGate {
    fn admit(&self, call: &ToolCall) -> Admission {
        ArgumentGate.admit(call)
    }

    fn admit_file_mutation(&self, mutation: &FileMutation) -> Admission {
        ArgumentGate.admit_file_mutation(mutation)
    }

    fn admit_mcp_tool(&self, call: &ToolCall) -> Admission {
        if self
            .remembered
            .lock()
            .unwrap()
            .contains(&format!("{:?}", SessionGrant::McpTool(call.name.clone())))
        {
            Admission::Allowed(PathAccess::WorkspaceOrExternal)
        } else {
            Admission::ApprovalRequired
        }
    }

    fn admit_command(&self, request: &CommandRequest) -> Admission {
        ArgumentGate.admit_command(request)
    }

    fn applicable_target(&self, call: &ToolCall) -> Option<ApplicableTarget> {
        ArgumentGate.applicable_target(call)
    }

    fn approval_scope(&self, action: GatedAction<'_>) -> ApprovalScope {
        let tree = approved_tree(self.scopes.fetch_add(1, Ordering::SeqCst) + 1);
        let always = match action {
            GatedAction::Call(_) => return tree,
            GatedAction::FileMutation(mutation) => mutation
                .target
                .parent()
                .map(|parent| SessionGrant::FileChangesUnder(parent.to_path_buf())),
            GatedAction::Command(_) => None,
            GatedAction::McpTool(call) => Some(SessionGrant::McpTool(call.name.clone())),
        };
        ApprovalScope { always, ..tree }
    }

    fn remember_approval(&self, grant: &SessionGrant) {
        self.remembered.lock().unwrap().push(format!("{grant:?}"));
    }

    fn forget_approvals(&self) {}
}

async fn run_approving<A: Into<ApprovalAnswer>>(
    provider: Arc<FakeProvider>,
    gate: Arc<RememberingGate>,
    decide: impl Fn(&ApprovalRequest) -> Option<A> + Sync,
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
            UiEvent::ApprovalRequested { request, .. } => Some((**request).clone()),
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
async fn approved_calls_run_with_the_scope_their_request_showed_and_always_remembers_it() {
    for (decision, remembered) in [
        (ApprovalDecision::Once, Vec::<String>::new()),
        (
            ApprovalDecision::Always,
            vec![r#"ReadsUnder("/approved/1")"#.to_owned()],
        ),
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
                call_id: ToolCallId::new("call-2"),
                tool_name: "echo".to_owned(),
                description: CallDescription {
                    title: r#"Echoing {"access":"outside"}"#.to_owned(),
                    label: None,
                    activity: ToolActivity::Read,
                    effect: ToolEffect::ReadOnly,
                    concurrency: Concurrency::Parallel,
                },
                tool_arguments_preview: r#"{"access":"outside"}"#.to_owned(),
                tool_arguments_truncated: false,
                scope: approved_tree(1),
                command: None,
                file: None,
                origin: ApprovalOrigin::ActiveSession,
                change: None,
            }]
        );
        assert_eq!(
            provider.requests()[1].messages[2..],
            [
                tool_message("call-1", "WorkspaceOnly", ToolResultStatus::Success),
                tool_message(
                    "call-2",
                    r#"Within("/approved/1")"#,
                    ToolResultStatus::Success
                ),
            ]
        );
        assert_eq!(gate.scopes.load(Ordering::SeqCst), 1);
        assert_eq!(*gate.remembered.lock().unwrap(), remembered);
    }
}

#[tokio::test]
async fn feedback_given_with_an_answer_follows_every_result_of_its_step() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"access":"outside","serial":true}"#),
            ("call-2", r#"{"access":"workspace","serial":true}"#),
            ("call-3", r#"{"path":"outside","serial":true}"#),
            ("call-4", r#"{"path":"outside again","serial":true}"#),
        ]),
        text_reply("done"),
    ]);
    let gate = Arc::new(RememberingGate::default());
    let (report, events, _) = run_approving(Arc::clone(&provider), gate, |request| {
        let (decision, feedback) = match request.call_id.as_str() {
            "call-1" => (ApprovalDecision::Once, "read the tests next"),
            "call-3" => (ApprovalDecision::Deny, "use the copy in the workspace"),
            _ => (ApprovalDecision::Once, ""),
        };
        Some(ApprovalAnswer {
            decision,
            feedback: Some(feedback.to_owned()),
        })
    })
    .await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let order: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ToolFinished { call_id, .. } => Some(format!("finish {call_id}")),
            UiEvent::ApprovalFeedback { text, .. } => Some(format!("feedback {text}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        [
            "finish call-1",
            "feedback read the tests next",
            "finish call-2",
            "finish call-3",
            "feedback use the copy in the workspace",
            "finish call-4",
        ]
    );
    let messages = &provider.requests()[1].messages[2..];
    assert_eq!(
        messages[4..],
        [
            ChatMessage::permission_feedback(ToolCallId::new("call-1"), "read the tests next"),
            ChatMessage::permission_feedback(
                ToolCallId::new("call-3"),
                "use the copy in the workspace"
            ),
        ]
    );
    assert!(
        messages[..4]
            .iter()
            .all(|message| matches!(message, ChatMessage::Tool { .. })),
        "{messages:?}"
    );
}

#[tokio::test]
async fn feedback_is_saved_with_the_result_it_follows() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"access":"outside","serial":true}"#),
            ("call-2", r#"{"access":"workspace","serial":true}"#),
        ]),
        text_reply("done"),
    ]);
    let approvals = Approvals::default();
    let (log, entries) = MemoryLog::shared();
    let shared: Arc<FakeProvider> = Arc::clone(&provider);
    let mut agent = logged(
        Agent::new(
            shared,
            vec![echo_tool()],
            Arc::new(FixedContext),
            Arc::new(RememberingGate::default()),
            config(),
        )
        .with_approvals(approvals.clone()),
        log,
    );
    let cancel = CancellationToken::new();
    agent
        .run_turn(
            "go",
            &mut |event| {
                if let UiEvent::ApprovalRequested { request, .. } = &event {
                    assert!(approvals.resolve(
                        request.id,
                        ApprovalAnswer {
                            decision: ApprovalDecision::Once,
                            feedback: Some("then read the tests".to_owned()),
                        }
                    ));
                }
            },
            &cancel,
        )
        .await;
    let entries = entries.lock().unwrap();
    let [Logged::Turn { steps, .. }] = entries.as_slice() else {
        panic!("{entries:?}");
    };
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert!(
        steps[0].contains(
            r#"call-1=Within(\"/approved/1\"):Success feedback=[\"then read the tests\"]"#
        ),
        "{steps:?}"
    );
    assert!(
        steps[0].contains(r#"call-2=WorkspaceOnly:Success""#),
        "{steps:?}"
    );
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
async fn file_changes_and_commands_ask_with_their_target_and_always_remembers_only_the_offer() {
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
        Some(if request.description.title.contains(r#""changes":2"#) {
            ApprovalDecision::Deny
        } else {
            ApprovalDecision::Always
        })
    })
    .await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = approval_requests(&events);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.scope.always.clone())
            .collect::<Vec<_>>(),
        [
            Some(SessionGrant::FileChangesUnder(PathBuf::from("/workspace"))),
            None,
            Some(SessionGrant::FileChangesUnder(PathBuf::from("/workspace"))),
        ]
    );
    let shown: Vec<(String, Option<CommandRequest>, Option<FileMutation>)> = requests
        .into_iter()
        .map(|request| (request.description.title, request.command, request.file))
        .collect();
    let note = || {
        Some(FileMutation {
            target: PathBuf::from("/workspace/note.txt"),
            state: FileMutationState::Changes,
        })
    };
    assert_eq!(
        shown,
        [
            (
                r#"Echoing {"changes":1,"serial":true}"#.to_owned(),
                None,
                note()
            ),
            (
                r#"Echoing {"stop":1,"serial":true}"#.to_owned(),
                Some(CommandRequest::Stop),
                None
            ),
            (
                r#"Echoing {"changes":2,"serial":true}"#.to_owned(),
                None,
                note()
            ),
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
        [r#"FileChangesUnder("/workspace")"#]
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
async fn file_change_requests_carry_a_copy_of_the_prepared_change_they_would_apply() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"changes":1,"previewed":1,"serial":true}"#),
            ("call-2", r#"{"changes":2,"serial":true}"#),
            ("call-3", r#"{"path":"outside","previewed":1}"#),
        ]),
        text_reply("done"),
    ]);
    let (_, events, _) = run_approving(provider, Arc::new(RememberingGate::default()), |_| {
        Some(ApprovalDecision::Deny)
    })
    .await;
    assert_eq!(
        approval_requests(&events)
            .into_iter()
            .map(|request| request.change)
            .collect::<Vec<_>>(),
        [
            Some(ProposedFileChange {
                display_path: "note.txt".to_owned(),
                before: Some(Arc::from(&b"before\n"[..])),
                after: Arc::from(&b"after\n"[..]),
            }),
            None,
            None,
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
        run_approving(provider, Arc::new(RememberingGate::default()), |_| {
            None::<ApprovalDecision>
        })
        .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(dispatch_order(&events), ["start call-1", "finish call-1"]);
    let request = approval_requests(&events).remove(0);
    assert!(!approvals.resolve(request.id, ApprovalDecision::Once));
}

#[tokio::test]
async fn requests_preview_the_arguments_terminal_safe_and_bounded_like_upstream() {
    let long = format!(
        "{{\"path\":\"outside\",\"text\":\"\u{7f}{}\"}}",
        "x".repeat(5000)
    );
    let provider = FakeProvider::new(vec![tool_reply(&[("call-1", &long)]), text_reply("done")]);
    let (_, events, _) = run_approving(provider, Arc::new(RememberingGate::default()), |_| {
        Some(ApprovalDecision::Deny)
    })
    .await;
    let request = approval_requests(&events).remove(0);
    assert!(request.tool_arguments_truncated);
    let preview = request.tool_arguments_preview;
    assert_eq!(preview.len(), MAX_TOOL_ARGUMENTS_PREVIEW_BYTES);
    assert!(
        preview.starts_with(r#"{"path":"outside","text":"\x7fxxx"#),
        "{preview}"
    );
    assert!(preview.ends_with("x..."), "{preview}");
}

#[tokio::test]
async fn an_answer_given_as_the_turn_is_cancelled_is_applied_and_never_dropped() {
    for cancel_first in [false, true] {
        let provider = FakeProvider::new(vec![tool_reply(&[("call-1", r#"{"path":"outside"}"#)])]);
        let gate = Arc::new(RememberingGate::default());
        let approvals = Approvals::default();
        let mut agent = Agent::new(
            provider,
            vec![echo_tool()],
            Arc::new(FixedContext),
            Arc::clone(&gate) as Arc<dyn PermissionGate>,
            config(),
        )
        .with_approvals(approvals.clone());
        let cancel = CancellationToken::new();
        let mut accepted = Vec::new();
        let mut events = Vec::new();
        let report = agent
            .run_turn(
                "go",
                &mut |event| {
                    if let UiEvent::ApprovalRequested { request, .. } = &event {
                        if cancel_first {
                            cancel.cancel();
                        }
                        accepted.push(approvals.resolve(request.id, ApprovalDecision::Always));
                        cancel.cancel();
                    }
                    events.push(event);
                },
                &cancel,
            )
            .await;
        assert_eq!(report.outcome, TurnOutcome::Interrupted, "{cancel_first}");
        assert!(dispatch_order(&events).is_empty(), "{cancel_first}");
        assert_eq!(accepted, [true], "{cancel_first}");
        assert_eq!(
            *gate.remembered.lock().unwrap(),
            [r#"ReadsUnder("/approved/1")"#],
            "{cancel_first}"
        );
    }
}

#[tokio::test]
async fn mcp_tool_calls_ask_through_their_own_gate_and_always_covers_the_tool() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"mcp_call":1,"serial":1}"#)]),
        tool_reply(&[("call-2", r#"{"mcp_call":2,"serial":1}"#)]),
        text_reply("done"),
    ]);
    let gate = Arc::new(RememberingGate::default());
    let (report, events, _) = run_approving(Arc::clone(&provider), Arc::clone(&gate), |_| {
        Some(ApprovalDecision::Always)
    })
    .await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = approval_requests(&events);
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].scope.always,
        Some(SessionGrant::McpTool("echo".to_owned()))
    );
    assert_eq!(
        requests[0].tool_arguments_preview,
        r#"{"mcp_call":1,"serial":1}"#
    );
    assert_eq!(
        *gate.remembered.lock().unwrap(),
        [r#"McpTool("echo")"#.to_owned()]
    );
    assert_eq!(started_titles(&events).len(), 2);
}

struct Flagging {
    spec: ToolSpec,
    ran: Arc<AtomicBool>,
}

struct FlagCall {
    arguments: String,
    ran: Arc<AtomicBool>,
}

impl Tool for Flagging {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        Ok(Box::new(FlagCall {
            arguments: arguments.to_owned(),
            ran: Arc::clone(&self.ran),
        }))
    }
}

impl PreparedCall for FlagCall {
    fn describe(&self) -> CallDescription {
        CallDescription {
            title: format!("Flagging {}", self.arguments),
            label: None,
            activity: ToolActivity::Read,
            effect: ToolEffect::ReadOnly,
            concurrency: Concurrency::Parallel,
        }
    }

    fn execute(self: Box<Self>, _context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        self.ran.store(true, Ordering::SeqCst);
        Box::pin(async move { ToolOutput::success(format!("flag {}", self.arguments)) })
    }
}

#[tokio::test]
async fn a_parallel_group_is_admitted_whole_before_any_of_its_calls_runs() {
    let provider = FakeProvider::new(vec![
        Script::Reply(
            Vec::new(),
            completion(
                None,
                vec![
                    ToolCall::new("call-1", "flag", r#"{"first":1}"#),
                    ToolCall::new("call-2", "flag", r#"{"outside":2}"#),
                ],
                FinishReason::ToolCalls,
            ),
        ),
        text_reply("done"),
    ]);
    let ran = Arc::new(AtomicBool::new(false));
    let tool: Arc<dyn Tool> = Arc::new(Flagging {
        spec: ToolSpec {
            name: "flag".to_owned(),
            description: "Flag.".to_owned(),
            input_schema: r#"{"type":"object"}"#.into(),
        },
        ran: Arc::clone(&ran),
    });
    let approvals = Approvals::default();
    let mut agent = Agent::new(
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        vec![tool],
        Arc::new(FixedContext),
        Arc::new(ArgumentGate),
        config(),
    )
    .with_permission_prompts(approvals.clone());
    let ran_while_asking = Arc::new(Mutex::new(None));
    let cancel = CancellationToken::new();
    let report = agent
        .run_turn(
            "go",
            &mut |event| {
                if let UiEvent::ApprovalRequested { request, .. } = &event {
                    let approvals = approvals.clone();
                    let ran = Arc::clone(&ran);
                    let seen = Arc::clone(&ran_while_asking);
                    let id = request.id;
                    tokio::spawn(async move {
                        for _ in 0..50 {
                            tokio::task::yield_now().await;
                        }
                        *seen.lock().unwrap() = Some(ran.load(Ordering::SeqCst));
                        approvals.resolve(id, ApprovalDecision::Once);
                    });
                }
            },
            &cancel,
        )
        .await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(*ran_while_asking.lock().unwrap(), Some(false));
    assert!(ran.load(Ordering::SeqCst));
    assert_eq!(
        provider.requests()[1].messages[2..],
        [r#"flag {"first":1}"#, r#"flag {"outside":2}"#]
            .into_iter()
            .zip(["call-1", "call-2"])
            .map(|(content, call_id)| ChatMessage::Tool {
                call_id: ToolCallId::new(call_id),
                tool_name: "flag".to_owned(),
                content: content.to_owned(),
                status: ToolResultStatus::Success,
            })
            .collect::<Vec<_>>()
    );
}
