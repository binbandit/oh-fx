use super::*;

struct PrepareCounter {
    inner: Arc<dyn Tool>,
    prepared: Arc<AtomicUsize>,
}

impl Tool for PrepareCounter {
    fn spec(&self) -> &ToolSpec {
        self.inner.spec()
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        self.prepared.fetch_add(1, Ordering::SeqCst);
        self.inner.prepare(arguments)
    }

    fn history_arguments(&self, arguments: &str) -> Option<String> {
        self.inner.history_arguments(arguments)
    }
}

struct Harness {
    provider: Arc<FakeProvider>,
    gate: Arc<RecordingGate>,
    prepared: Arc<AtomicUsize>,
    agent: Agent,
}

fn harness(scripts: Vec<Script>) -> Harness {
    let provider = FakeProvider::new(scripts);
    let gate = Arc::new(RecordingGate::default());
    let prepared = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(PrepareCounter {
        inner: echo_tool(),
        prepared: Arc::clone(&prepared),
    });
    let agent = Agent::new(
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        vec![tool],
        Arc::new(FixedContext),
        Arc::clone(&gate) as Arc<dyn PermissionGate>,
        config(),
    );
    Harness {
        provider,
        gate,
        prepared,
        agent,
    }
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

fn history_calls(messages: &[ChatMessage]) -> Vec<&ToolCall> {
    messages
        .iter()
        .flat_map(|message| match message {
            ChatMessage::Assistant { tool_calls, .. } => tool_calls.as_slice(),
            _ => &[],
        })
        .collect()
}

fn started_calls(events: &[UiEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, UiEvent::ToolStarted { .. }))
        .count()
}

fn non_object(tool_name: &str) -> String {
    non_object_tool_arguments_json(tool_name)
}

fn diagnosed(tool_name: &str, raw: &str) -> String {
    malformed_tool_arguments_json(tool_name, &ToolArgumentDiagnostic::diagnose(raw))
}

#[tokio::test]
async fn non_object_function_arguments_are_blocked_before_hooks_with_safe_replay_input() {
    let rejected = ["[]", "[1]", "42", "null", "true", "\"text\""];
    let valid = r#" {"items":[1,{"nested":true}]} "#;
    let ids: Vec<String> = (0..rejected.len())
        .map(|index| format!("rejected-{index}"))
        .collect();
    let mut calls: Vec<(&str, &str)> = ids.iter().map(String::as_str).zip(rejected).collect();
    calls.push(("valid", valid));
    let mut harness = harness(vec![tool_reply(&calls), text_reply("ok")]);
    let (report, events) = run(&mut harness.agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(harness.prepared.load(Ordering::SeqCst), 1);
    let expected: Vec<Rejected<'_>> = ids
        .iter()
        .map(|id| {
            (
                id.as_str(),
                "echo",
                "{}",
                ToolRejection::MalformedArguments,
                None,
            )
        })
        .collect();
    assert_eq!(rejections(&events), expected);
    let messages = &harness.provider.requests()[1].messages;
    let replayed: Vec<(&str, &str, &str)> = history_calls(messages)
        .into_iter()
        .map(|call| {
            (
                call.id.as_str(),
                call.name.as_str(),
                call.arguments.as_str(),
            )
        })
        .collect();
    let mut expected_replay: Vec<(&str, &str, &str)> =
        ids.iter().map(|id| (id.as_str(), "echo", "{}")).collect();
    expected_replay.push(("valid", "echo", valid));
    assert_eq!(replayed, expected_replay);
    let results = tool_results(messages);
    for (index, id) in ids.iter().enumerate() {
        assert_eq!(
            results[index],
            (
                id.as_str(),
                non_object("echo").as_str(),
                ToolResultStatus::Failure
            )
        );
        assert!(results[index].1.contains("object"));
    }
    assert_eq!(results[rejected.len()].1, format!("echo {valid}"));
}

#[tokio::test]
async fn malformed_function_arguments_report_the_diagnosed_input_they_replaced() {
    let raw = r#"{"path":"src/main.zig","offset":"#;
    let mut harness = harness(vec![tool_reply(&[("raw", raw)]), text_reply("ok")]);
    let (report, events) = run(&mut harness.agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        rejections(&events),
        [("raw", "echo", "{}", ToolRejection::MalformedArguments, None)]
    );
    let messages = &harness.provider.requests()[1].messages;
    assert_eq!(history_calls(messages), [&echo_call("raw", "{}")]);
    let results = tool_results(messages);
    assert_eq!(
        results,
        [(
            "raw",
            diagnosed("echo", raw).as_str(),
            ToolResultStatus::Failure
        )]
    );
    let model_output = results[0].1;
    assert!(!model_output.contains(raw));
    assert!(model_output.contains(&format!(
        r#""details":{{"failure":"truncated","received_bytes":{0},"error_offset":{0}}}"#,
        raw.len()
    )));
}

#[tokio::test]
async fn process_queued_prompt_recovers_malformed_local_arguments_before_tool_semantics() {
    let raw = r#"{"path":"SHOULD_NOT_SURVIVE""#;
    let mut harness = harness(vec![
        Script::Reply(
            vec![StreamEvent::TextDelta {
                text: "I'll inspect it.".to_owned(),
            }],
            completion(
                Some("I'll inspect it."),
                vec![echo_call("call_read", raw)],
                FinishReason::ToolCalls,
            ),
        ),
        text_reply("Recovered."),
    ]);
    let (report, events) = run(&mut harness.agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "Recovered.");
    assert_eq!(harness.provider.requests().len(), 2);
    assert_eq!(harness.prepared.load(Ordering::SeqCst), 0);
    assert!(harness.gate.admitted.lock().unwrap().is_empty());
    assert_eq!(started_calls(&events), 0);
    assert!(finished(&events).is_empty());
    assert_eq!(rejections(&events).len(), 1);
    let messages = &harness.provider.requests()[1].messages;
    assert_eq!(
        messages[1..],
        [
            ChatMessage::Assistant {
                content: Some("I'll inspect it.".to_owned()),
                tool_calls: vec![echo_call("call_read", "{}")],
                provider_replay: None,
            },
            tool_message(
                "call_read",
                &diagnosed("echo", raw),
                ToolResultStatus::Failure
            ),
        ]
    );
    assert!(!format!("{messages:?}").contains("SHOULD_NOT_SURVIVE"));
}

#[tokio::test]
async fn process_queued_prompt_malformed_parallel_call_preserves_valid_sibling_exactly_once() {
    let mut harness = harness(vec![
        tool_reply(&[
            ("call_fetch", r#"{"url":"#),
            ("call_read", r#"{"text":"README.md"}"#),
        ]),
        text_reply("Final"),
    ]);
    let (report, events) = run(&mut harness.agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(harness.prepared.load(Ordering::SeqCst), 1);
    assert_eq!(*harness.gate.admitted.lock().unwrap(), ["echo"]);
    assert_eq!(
        dispatch_order(&events),
        ["start call_read", "finish call_read"]
    );
    assert_eq!(
        finished(&events),
        [("call_read", ToolResultStatus::Success)]
    );
    assert_eq!(
        rejections(&events),
        [(
            "call_fetch",
            "echo",
            "{}",
            ToolRejection::MalformedArguments,
            None
        )]
    );
    let messages = &harness.provider.requests()[1].messages;
    assert_eq!(
        tool_results(messages),
        [
            (
                "call_fetch",
                diagnosed("echo", r#"{"url":"#).as_str(),
                ToolResultStatus::Failure
            ),
            (
                "call_read",
                r#"echo {"text":"README.md"}"#,
                ToolResultStatus::Success
            ),
        ]
    );
}

#[tokio::test]
async fn process_queued_prompt_stops_repeated_malformed_calls_before_another_provider_request() {
    let mut harness = harness(vec![
        tool_reply(&[("call_1", "{")]),
        tool_reply(&[("call_2", "{")]),
        tool_reply(&[("call_3", "{")]),
        text_reply("must not be requested"),
    ]);
    let (report, events) = run(&mut harness.agent, "go").await;
    assert_eq!(harness.provider.requests().len(), 3);
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(
        report.failure,
        Some(TurnFailure::RepeatedMalformedArguments)
    );
    assert_eq!(report.final_text, "");
    let operational: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::Operational { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        operational,
        [format!("{REPEATED_MALFORMED_ARGUMENTS_NOTICE}\n")]
    );
    assert_eq!(
        harness.agent.history.last(),
        Some(&ChatMessage::Assistant {
            content: Some(REPEATED_MALFORMED_ARGUMENTS_NOTICE.to_owned()),
            tool_calls: Vec::new(),
            provider_replay: None,
        })
    );
    let history = &harness.agent.history;
    let replayed: Vec<(&str, &str)> = history_calls(history)
        .into_iter()
        .map(|call| (call.id.as_str(), call.arguments.as_str()))
        .collect();
    assert_eq!(
        replayed,
        [("call_1", "{}"), ("call_2", "{}"), ("call_3", "{}")]
    );
    let expected = diagnosed("echo", "{");
    assert_eq!(
        tool_results(history),
        [
            ("call_1", expected.as_str(), ToolResultStatus::Failure),
            ("call_2", expected.as_str(), ToolResultStatus::Failure),
            ("call_3", expected.as_str(), ToolResultStatus::Failure),
        ]
    );
    assert!(harness.gate.admitted.lock().unwrap().is_empty());
    assert_eq!(harness.prepared.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_arguments_retry_state_stops_consecutive_all_malformed_batches() {
    let valid = r#"{"text":"README.md"}"#;
    let mut harness = harness(vec![
        tool_reply(&[("read-1", "{")]),
        tool_reply(&[("fetch-1", "[]")]),
        tool_reply(&[("fetch-2", "[]"), ("read-valid", valid)]),
        tool_reply(&[("read-2", "{")]),
        tool_reply(&[("read-3", "{")]),
        tool_reply(&[("read-4", "{")]),
        text_reply("must not be requested"),
    ]);
    let (report, _) = run(&mut harness.agent, "go").await;
    assert_eq!(harness.provider.requests().len(), 6);
    assert_eq!(
        report.failure,
        Some(TurnFailure::RepeatedMalformedArguments)
    );
    assert_eq!(harness.prepared.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn non_object_arguments_use_the_existing_bounded_invalid_argument_retry_budget() {
    let mut harness = harness(vec![
        tool_reply(&[("bad-1", "[]")]),
        tool_reply(&[("bad-2", "[]")]),
        tool_reply(&[("bad-3", "[]")]),
        tool_reply(&[("bad-4", "[]")]),
        tool_reply(&[("good", "{}")]),
        tool_reply(&[("bad-5", "[]")]),
        text_reply("recovered"),
    ]);
    let (first, _) = run(&mut harness.agent, "go").await;
    assert_eq!(first.failure, Some(TurnFailure::RepeatedMalformedArguments));
    assert_eq!(harness.provider.requests().len(), 3);
    let (second, _) = run(&mut harness.agent, "again").await;
    assert_eq!(second.outcome, TurnOutcome::Completed);
    assert_eq!(second.final_text, "recovered");
    assert_eq!(harness.provider.requests().len(), 7);
}

#[tokio::test]
async fn repeated_malformed_calls_never_escalate_as_identical_failures() {
    let mut harness = harness(vec![
        tool_reply(&[("call-1", "[]")]),
        tool_reply(&[("call-2", "[]"), ("call-3", r#"{"fail":1}"#)]),
        tool_reply(&[("call-4", "[]"), ("call-5", r#"{"fail":1}"#)]),
        text_reply("done"),
    ]);
    let (report, _) = run(&mut harness.agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let messages = &harness.provider.requests()[3].messages;
    let rejected = non_object("echo");
    assert_eq!(
        tool_results(messages),
        [
            ("call-1", rejected.as_str(), ToolResultStatus::Failure),
            ("call-2", rejected.as_str(), ToolResultStatus::Failure),
            ("call-3", "echo failed", ToolResultStatus::Failure),
            ("call-4", rejected.as_str(), ToolResultStatus::Failure),
            (
                "call-5",
                "echo failed\n\nThis exact call has already failed 2 times this turn with the same arguments. Do not retry it unchanged.",
                ToolResultStatus::Failure
            ),
        ]
    );
}

#[tokio::test]
async fn malformed_arguments_are_judged_before_the_tool_is_looked_up() {
    let provider = FakeProvider::new(vec![
        Script::Reply(
            Vec::new(),
            completion(
                None,
                vec![
                    ToolCall {
                        id: ToolCallId::new("call-1"),
                        name: "missing".to_owned(),
                        arguments: "[]".to_owned(),
                    },
                    ToolCall {
                        id: ToolCallId::new("call-2"),
                        name: "missing".to_owned(),
                        arguments: "{}".to_owned(),
                    },
                ],
                FinishReason::ToolCalls,
            ),
        ),
        text_reply("ok"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        rejections(&events),
        [
            (
                "call-1",
                "missing",
                "{}",
                ToolRejection::MalformedArguments,
                None
            ),
            (
                "call-2",
                "missing",
                "{}",
                ToolRejection::Unsupported,
                Some("Working: missing")
            ),
        ]
    );
    let messages = &provider.requests()[1].messages;
    let results: Vec<&str> = tool_results(messages)
        .into_iter()
        .map(|(_, content, _)| content)
        .collect();
    assert_eq!(
        results,
        [non_object("missing").as_str(), "Unsupported tool: missing"]
    );
}
