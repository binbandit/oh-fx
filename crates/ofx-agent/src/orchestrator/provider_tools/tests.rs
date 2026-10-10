use super::*;

fn provider_call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        provider_result: Some("{}".to_owned()),
        provenance: ToolExecutionProvenance::ProviderExecuted,
        ..ToolCall::new(id, name, r#"{"query":"PROVIDER_ARGUMENT_SECRET"}"#)
    }
}

fn step(calls: Vec<ToolCall>) -> ChatMessage {
    ChatMessage::Assistant {
        content: None,
        tool_calls: calls,
        provider_replay: None,
    }
}

fn result(call: &ToolCall, status: ToolResultStatus) -> ChatMessage {
    ChatMessage::Tool {
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: "PROVIDER_RESULT_SECRET".to_owned(),
        status,
    }
}

#[test]
fn retained_web_searches_keep_provider_searches_with_their_result_status() {
    let exa = provider_call("call_exa", "exa_search");
    let parallel = provider_call("call_parallel", "parallel_search");
    let pending = provider_call("call_pending", "perplexity_search");
    let local = ToolCall::new("call_local", "read_file", "{}");
    let fetched = provider_call("call_fetch", "web_fetch");
    let history = [
        ChatMessage::user("search"),
        step(vec![
            exa.clone(),
            parallel.clone(),
            pending,
            local.clone(),
            fetched.clone(),
        ]),
        result(&parallel, ToolResultStatus::Failure),
        result(&exa, ToolResultStatus::Success),
        result(&local, ToolResultStatus::Success),
        result(&fetched, ToolResultStatus::Success),
    ];
    assert_eq!(
        retained_web_searches(&history),
        [
            Some(ToolResultStatus::Success),
            Some(ToolResultStatus::Failure),
            None,
        ]
    );
}

#[test]
fn retained_web_searches_keep_the_most_recent_bounded_calls() {
    let calls: Vec<ToolCall> = (0..=RING_CAPACITY)
        .map(|index| provider_call(&format!("call_{index}"), "exa_search"))
        .collect();
    let oldest = result(&calls[0], ToolResultStatus::Failure);
    let newest = result(&calls[RING_CAPACITY], ToolResultStatus::Failure);
    let history = [ChatMessage::user("search"), step(calls), oldest, newest];
    let searches = retained_web_searches(&history);
    assert_eq!(searches.len(), RING_CAPACITY);
    assert_eq!(searches[0], None);
    assert_eq!(searches[RING_CAPACITY - 1], Some(ToolResultStatus::Failure));
    assert_eq!(searches.iter().filter(|status| status.is_some()).count(), 1);
}

#[test]
fn a_reused_call_id_settles_the_latest_open_search() {
    let first = provider_call("ws_1", "exa_search");
    let again = provider_call("ws_1", "exa_search");
    let history = [
        ChatMessage::user("search"),
        step(vec![first.clone()]),
        result(&first, ToolResultStatus::Success),
        ChatMessage::user("again"),
        step(vec![again.clone()]),
        result(&again, ToolResultStatus::Failure),
    ];
    assert_eq!(
        retained_web_searches(&history),
        [
            Some(ToolResultStatus::Success),
            Some(ToolResultStatus::Failure),
        ]
    );
}
