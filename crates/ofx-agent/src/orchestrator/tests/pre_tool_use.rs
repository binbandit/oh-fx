use std::sync::Mutex;

use ofx_contract::{
    HookHandlerError, HookRuntime, HookScope, PreToolUseAction, pre_tool_use_blocked_json,
    pre_tool_use_failed_closed_json,
};

use super::*;

type Seen = Arc<Mutex<Vec<(String, String, usize)>>>;
type Action = fn(&str) -> Result<PreToolUseAction, HookHandlerError>;

fn hooked(
    agent: Agent,
    action: impl Fn(&str) -> Result<PreToolUseAction, HookHandlerError> + Send + Sync + 'static,
) -> (Agent, Seen) {
    let seen = Seen::default();
    let recorded = Arc::clone(&seen);
    let mut hooks = HookRuntime::default();
    hooks
        .register_pre_tool_use("test.pre_tool_use", move |input| {
            recorded.lock().unwrap().push((
                input.call_id.to_owned(),
                input.arguments_json.to_owned(),
                input.step_index,
            ));
            action(input.arguments_json)
        })
        .unwrap();
    (
        agent.with_lifecycle(hooks.freeze(), HookScope::Interactive),
        seen,
    )
}

fn gated_agent(provider: &Arc<FakeProvider>, gate: &Arc<RecordingGate>) -> Agent {
    Agent::new(
        Arc::clone(provider) as Arc<dyn ModelProvider>,
        vec![echo_tool()],
        Arc::new(FixedContext),
        Arc::clone(gate) as Arc<dyn PermissionGate>,
        config(),
    )
}

fn tool_results(messages: &[ChatMessage]) -> Vec<(&str, &str, ToolResultStatus)> {
    messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::Tool {
                call_id,
                content,
                status,
                ..
            } => Some((call_id.as_str(), content.as_str(), *status)),
            _ => None,
        })
        .collect()
}

fn issued(messages: &[ChatMessage]) -> Vec<(&str, &str)> {
    messages
        .iter()
        .flat_map(|message| match message {
            ChatMessage::Assistant { tool_calls, .. } => tool_calls.as_slice(),
            _ => &[],
        })
        .map(|call| (call.id.as_str(), call.arguments.as_str()))
        .collect()
}

#[tokio::test]
async fn a_rewrite_is_what_validation_permission_execution_and_history_see() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"text":"original"}"#),
            ("call-2", r#"{"text":"keep"}"#),
        ]),
        text_reply("done"),
    ]);
    let gate = Arc::new(RecordingGate::default());
    let (mut agent, seen) = hooked(gated_agent(&provider, &gate), |arguments| {
        Ok(if arguments.contains("original") {
            PreToolUseAction::RewriteArguments(r#"{"text":"rewritten"}"#.to_owned())
        } else {
            PreToolUseAction::Continue
        })
    });
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        *seen.lock().unwrap(),
        [
            ("call-1".to_owned(), r#"{"text":"original"}"#.to_owned(), 1),
            ("call-2".to_owned(), r#"{"text":"keep"}"#.to_owned(), 1),
        ]
    );
    assert_eq!(*gate.admitted.lock().unwrap(), ["echo", "echo"]);
    let started: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ToolStarted { description, .. } => Some(description.title.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        [
            r#"Echoing {"text":"rewritten"}"#,
            r#"Echoing {"text":"keep"}"#
        ]
    );
    let follow_up = &provider.requests()[1].messages;
    assert_eq!(
        issued(follow_up),
        [
            ("call-1", r#"{"text":"rewritten"}"#),
            ("call-2", r#"{"text":"keep"}"#)
        ]
    );
    assert_eq!(
        tool_results(follow_up),
        [
            (
                "call-1",
                r#"echo {"text":"rewritten"}"#,
                ToolResultStatus::Success
            ),
            (
                "call-2",
                r#"echo {"text":"keep"}"#,
                ToolResultStatus::Success
            ),
        ]
    );
}

#[tokio::test]
async fn a_rewrite_reaches_the_tools_own_validation() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"text":"fine"}"#)]),
        text_reply("done"),
    ]);
    let gate = Arc::new(RecordingGate::default());
    let (mut agent, _) = hooked(gated_agent(&provider, &gate), |_| {
        Ok(PreToolUseAction::RewriteArguments(
            r#"{"invalid":true}"#.to_owned(),
        ))
    });
    let (_, events) = run(&mut agent, "go").await;
    assert_eq!(
        rejections(&events),
        [(
            "call-1",
            "echo",
            r#"{"invalid":true}"#,
            ToolRejection::Invalid,
            None
        )]
    );
    assert!(gate.admitted.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_block_is_the_calls_failed_result_and_nothing_else_runs() {
    let reason = "Blocked by the workspace lifecycle policy.";
    let original = r#"{"path":"secrets.txt"}"#;
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", original)]),
        tool_reply(&[("call-2", original)]),
        text_reply("I will use another approach."),
    ]);
    let gate = Arc::new(RecordingGate::default());
    let (mut agent, seen) = hooked(gated_agent(&provider, &gate), move |_| {
        Ok(PreToolUseAction::Block(reason.to_owned()))
    });
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert!(gate.admitted.lock().unwrap().is_empty());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, UiEvent::ToolStarted { .. }))
    );
    assert_eq!(
        rejections(&events),
        [
            ("call-1", "echo", original, ToolRejection::Invalid, None),
            ("call-2", "echo", original, ToolRejection::Invalid, None),
        ]
    );
    let blocked = pre_tool_use_blocked_json("echo", reason);
    let follow_up = &provider.requests()[2].messages;
    assert_eq!(
        issued(follow_up),
        [("call-1", original), ("call-2", original)]
    );
    assert_eq!(
        tool_results(follow_up),
        [
            ("call-1", blocked.as_str(), ToolResultStatus::Failure),
            (
                "call-2",
                format!(
                    "{blocked}\n\nThis exact call has already failed 2 times this turn with the same arguments. Do not retry it unchanged."
                )
                .as_str(),
                ToolResultStatus::Failure
            ),
        ]
    );
}

#[tokio::test]
async fn handler_failures_and_invalid_outputs_block_the_call_closed() {
    let actions: [Action; 4] = [
        |_| Err(HookHandlerError::Failed),
        |_| Err(HookHandlerError::Cancelled),
        |_| Ok(PreToolUseAction::RewriteArguments("[]".to_owned())),
        |_| Ok(PreToolUseAction::Block(String::new())),
    ];
    for action in actions {
        let provider = FakeProvider::new(vec![
            tool_reply(&[("call-1", r#"{"text":"a"}"#)]),
            text_reply("done"),
        ]);
        let gate = Arc::new(RecordingGate::default());
        let (mut agent, _) = hooked(gated_agent(&provider, &gate), action);
        let (report, _) = run(&mut agent, "go").await;
        assert_eq!(report.outcome, TurnOutcome::Completed);
        assert!(gate.admitted.lock().unwrap().is_empty());
        assert_eq!(
            tool_results(&provider.requests()[1].messages),
            [(
                "call-1",
                pre_tool_use_failed_closed_json("echo").as_str(),
                ToolResultStatus::Failure
            )]
        );
    }
}

#[tokio::test]
async fn malformed_and_non_object_calls_are_rejected_before_the_hook() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"text":"#), ("call-2", "[]")]),
        text_reply("done"),
    ]);
    let gate = Arc::new(RecordingGate::default());
    let (mut agent, seen) = hooked(gated_agent(&provider, &gate), |_| {
        Ok(PreToolUseAction::Continue)
    });
    let (_, events) = run(&mut agent, "go").await;
    assert!(seen.lock().unwrap().is_empty());
    let reasons: Vec<ToolRejection> = rejections(&events)
        .into_iter()
        .map(|(_, _, _, reason, _)| reason)
        .collect();
    assert_eq!(
        reasons,
        [
            ToolRejection::MalformedArguments,
            ToolRejection::MalformedArguments
        ]
    );
}

#[tokio::test]
async fn cancelling_while_the_hook_runs_interrupts_before_the_tool_step_is_kept() {
    let provider = FakeProvider::new(vec![tool_reply(&[("call-1", r#"{"text":"a"}"#)])]);
    let gate = Arc::new(RecordingGate::default());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let (mut agent, seen) = hooked(gated_agent(&provider, &gate), move |_| {
        trigger.cancel();
        Ok(PreToolUseAction::Continue)
    });
    let report = agent.run_turn("go", &mut |_| {}, &cancel).await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(gate.admitted.lock().unwrap().is_empty());
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(agent.history, [ChatMessage::user("go")]);
}
