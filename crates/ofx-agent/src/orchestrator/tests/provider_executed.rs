use ofx_contract::ToolExecutionProvenance;

use super::*;

const SOURCE: &str = r#"{"results":[{"url":"https://example.test/source"}]}"#;

fn provider_call(id: &str, name: &str, provider_result: Option<&str>) -> ToolCall {
    ToolCall {
        provider_result: provider_result.map(str::to_owned),
        provenance: ToolExecutionProvenance::ProviderExecuted,
        ..ToolCall::new(id, name, "{}")
    }
}

fn reply(content: Option<&str>, calls: Vec<ToolCall>, finish_reason: FinishReason) -> Script {
    Script::Reply(Vec::new(), completion(content, calls, finish_reason))
}

struct Harness {
    provider: Arc<FakeProvider>,
    gate: Arc<RecordingGate>,
    agent: Agent,
}

fn harness(scripts: Vec<Script>) -> Harness {
    let provider = FakeProvider::new(scripts);
    let gate = Arc::new(RecordingGate::default());
    let agent = Agent::new(
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        vec![echo_tool()],
        Arc::new(FixedContext),
        Arc::clone(&gate) as Arc<dyn PermissionGate>,
        config(),
    );
    Harness {
        provider,
        gate,
        agent,
    }
}

fn tool_messages(messages: &[ChatMessage]) -> Vec<(&str, &str, ToolResultStatus)> {
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

#[tokio::test]
async fn provider_executed_calls_publish_their_provider_result_instead_of_running() {
    let search = provider_call("provider_search", "exa_search", Some(SOURCE));
    let calls = vec![
        echo_call("call-1", r#"{"text":"a"}"#),
        search.clone(),
        echo_call("call-2", r#"{"text":"b"}"#),
    ];
    let mut harness = harness(vec![
        reply(None, calls.clone(), FinishReason::ToolCalls),
        text_reply("Final"),
    ]);
    let (report, events) = run(&mut harness.agent, "search").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(*harness.gate.admitted.lock().unwrap(), ["echo", "echo"]);
    assert_eq!(
        dispatch_order(&events),
        [
            "start call-1",
            "finish call-1",
            "start call-2",
            "finish call-2"
        ]
    );
    let requests = harness.provider.requests();
    let messages = &requests[1].messages;
    assert_eq!(
        messages[1],
        ChatMessage::Assistant {
            content: None,
            tool_calls: calls,
            provider_replay: None,
        }
    );
    assert_eq!(
        tool_messages(messages),
        [
            ("call-1", r#"echo {"text":"a"}"#, ToolResultStatus::Success),
            ("provider_search", SOURCE, ToolResultStatus::Success),
            ("call-2", r#"echo {"text":"b"}"#, ToolResultStatus::Success),
        ]
    );
}

#[tokio::test]
async fn provider_results_that_report_a_tool_failure_are_failures() {
    let structured = r#"{"error":{"type":"tool_execution_failed","tool_name":"perplexity_search","message":"search failed"}}"#;
    let legacy = "Tool perplexity_search failed: upstream timeout";
    let adapter = "read_file failed: missing.txt";
    let other_type = r#"{"error":{"type":"tool_permission_denied"}}"#;
    let calls = vec![
        provider_call("structured", "perplexity_search", Some(structured)),
        provider_call("legacy", "perplexity_search", Some(legacy)),
        provider_call("adapter", "perplexity_search", Some(adapter)),
        provider_call("other", "perplexity_search", Some(other_type)),
        provider_call("plain", "perplexity_search", Some("[]")),
    ];
    let mut harness = harness(vec![
        reply(None, calls, FinishReason::ToolCalls),
        text_reply("Final"),
    ]);
    let (report, _) = run(&mut harness.agent, "search").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = harness.provider.requests();
    let statuses: Vec<(&str, ToolResultStatus)> = tool_messages(&requests[1].messages)
        .into_iter()
        .map(|(id, _, status)| (id, status))
        .collect();
    assert_eq!(
        statuses,
        [
            ("structured", ToolResultStatus::Failure),
            ("legacy", ToolResultStatus::Failure),
            ("adapter", ToolResultStatus::Success),
            ("other", ToolResultStatus::Success),
            ("plain", ToolResultStatus::Success),
        ]
    );
}

#[tokio::test]
async fn a_stop_with_provider_results_keeps_them_and_finishes_without_another_request() {
    let search = provider_call("provider_search", "perplexity_search", Some(SOURCE));
    let final_text = "Final [source](https://example.test/source)";
    let mut harness = harness(vec![Script::Reply(
        vec![StreamEvent::TextDelta {
            text: final_text.to_owned(),
        }],
        completion(Some(final_text), vec![search.clone()], FinishReason::Stop),
    )]);
    let (report, _) = run(&mut harness.agent, "search").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, final_text);
    assert_eq!(harness.provider.requests().len(), 1);
    assert!(harness.gate.admitted.lock().unwrap().is_empty());
    assert_eq!(
        harness.agent.history,
        [
            ChatMessage::user("search"),
            ChatMessage::Assistant {
                content: None,
                tool_calls: vec![search],
                provider_replay: None,
            },
            ChatMessage::Tool {
                call_id: ToolCallId::new("provider_search"),
                tool_name: "perplexity_search".to_owned(),
                content: SOURCE.to_owned(),
                status: ToolResultStatus::Success,
            },
            ChatMessage::Assistant {
                content: Some(final_text.to_owned()),
                tool_calls: Vec::new(),
                provider_replay: None,
            },
        ]
    );
}

#[tokio::test]
async fn a_stop_that_also_holds_local_calls_is_still_invalid() {
    let calls = vec![
        provider_call("provider_search", "exa_search", Some(SOURCE)),
        echo_call("call-1", "{}"),
    ];
    let mut harness = harness(vec![reply(Some("Final"), calls, FinishReason::Stop)]);
    let (report, _) = run(&mut harness.agent, "search").await;
    assert_eq!(report.failure, Some(TurnFailure::InvalidCompletion));
}

#[tokio::test]
async fn provider_executed_arguments_are_kept_as_sent() {
    let search = ToolCall {
        arguments: "[]".to_owned(),
        ..provider_call("provider_search", "exa_search", Some(SOURCE))
    };
    let mut harness = harness(vec![
        reply(None, vec![search.clone()], FinishReason::ToolCalls),
        text_reply("Final"),
    ]);
    let (report, events) = run(&mut harness.agent, "search").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, UiEvent::ToolRejected { .. }))
    );
    assert_eq!(
        harness.provider.requests()[1].messages[1],
        ChatMessage::Assistant {
            content: None,
            tool_calls: vec![search],
            provider_replay: None,
        }
    );
}

#[tokio::test]
async fn malformed_provider_calls_fail_the_turn_before_any_call_runs() {
    let missing = provider_call("provider_search", "exa_search", None);
    let malformed = ToolCall {
        arguments: r#"{"query":"#.to_owned(),
        ..provider_call("provider_read", "read_file", Some(SOURCE))
    };
    for (calls, failure) in [
        (
            vec![echo_call("call-1", "{}"), missing.clone()],
            TurnFailure::MalformedProviderResult,
        ),
        (
            vec![echo_call("call-1", "{}"), malformed.clone()],
            TurnFailure::MalformedProviderArguments,
        ),
        (
            vec![missing, malformed],
            TurnFailure::MalformedProviderArguments,
        ),
    ] {
        let mut harness = harness(vec![reply(None, calls, FinishReason::ToolCalls)]);
        let (report, events) = run(&mut harness.agent, "search").await;
        assert_eq!(report.outcome, TurnOutcome::Failed);
        assert_eq!(report.failure, Some(failure));
        assert!(harness.gate.admitted.lock().unwrap().is_empty());
        assert!(dispatch_order(&events).is_empty());
        assert!(harness.agent.history.is_empty());
    }
    assert_eq!(
        TurnFailure::MalformedProviderResult.code(),
        "MalformedProviderResultIdentity"
    );
    assert_eq!(
        TurnFailure::MalformedProviderArguments.code(),
        "MalformedProviderToolArguments"
    );
}

#[tokio::test]
async fn calls_that_carry_a_provider_result_never_join_a_parallel_group() {
    let answered = ToolCall {
        provider_result: Some(SOURCE.to_owned()),
        ..echo_call("call-2", r#"{"text":"b"}"#)
    };
    let calls = vec![
        echo_call("call-1", r#"{"text":"a"}"#),
        answered,
        echo_call("call-3", r#"{"text":"c"}"#),
        echo_call("call-4", r#"{"text":"d"}"#),
    ];
    let mut harness = harness(vec![
        reply(None, calls, FinishReason::ToolCalls),
        text_reply("Final"),
    ]);
    let (report, events) = run(&mut harness.agent, "read").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        dispatch_order(&events),
        [
            "start call-1",
            "finish call-1",
            "start call-2",
            "finish call-2",
            "start call-3",
            "start call-4",
            "finish call-3",
            "finish call-4",
        ]
    );
    let requests = harness.provider.requests();
    assert_eq!(
        tool_messages(&requests[1].messages)[1],
        ("call-2", r#"echo {"text":"b"}"#, ToolResultStatus::Success)
    );
}

#[tokio::test]
async fn a_stop_with_only_provider_results_and_no_answer_asks_again() {
    let search = provider_call("provider_search", "exa_search", Some(SOURCE));
    let mut harness = harness(vec![
        reply(None, vec![search], FinishReason::Stop),
        text_reply("Final"),
    ]);
    let (report, _) = run(&mut harness.agent, "search").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "Final");
    let requests = harness.provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        tool_messages(&requests[1].messages),
        [("provider_search", SOURCE, ToolResultStatus::Success)]
    );
}

#[tokio::test]
async fn a_final_answer_with_provider_results_splits_its_replay_between_step_and_answer() {
    let search = provider_call("provider_search", "perplexity_search", Some(SOURCE));
    let mut harness = harness(vec![with_replay(
        reply(Some("Final"), vec![search], FinishReason::Stop),
        "parts",
    )]);
    let (report, _) = run(&mut harness.agent, "search").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        harness.provider.projections(),
        [
            ("parts".to_owned(), false, true),
            ("parts".to_owned(), true, false),
        ]
    );
    assert_eq!(
        replays(&harness.agent.history),
        [Some("reasoning of parts"), Some("reasoning of parts")]
    );
}
