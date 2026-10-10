use ofx_contract::{Completion, ProviderError, ProviderErrorKind, ToolCall, ToolSpec, Usage};
use ofx_trace::{NetworkRing, Ring, TraceContext};

use super::*;
use crate::compactor::trace::{CompactionEvent, CompactionTraceKind};
use crate::scripted_provider::{ScriptedProvider, calling, failure, text};

const CONTEXT: TraceContext = TraceContext {
    turn_id: 4,
    step_id: 9,
    subagent_id: 0,
};

fn ring() -> &'static Ring<CompactionEvent> {
    Box::leak(Box::new(Ring::new(64)))
}

fn traced(ring: &'static Ring<CompactionEvent>) -> Vec<(CompactionTraceKind, bool, String)> {
    ring.snapshot()
        .into_iter()
        .map(|event| (event.event.kind, event.event.failed, event.event.detail))
        .collect()
}

fn efforts(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn summarizer<'a>(
    provider: &'a ScriptedProvider,
    reasoning_efforts: &'a [String],
    conversation: Option<ModelRequest<'a>>,
    cancel: &'a CancellationToken,
) -> Summarizer<'a> {
    traced_summarizer(provider, reasoning_efforts, conversation, cancel, ring())
}

fn traced_summarizer<'a>(
    provider: &'a ScriptedProvider,
    reasoning_efforts: &'a [String],
    conversation: Option<ModelRequest<'a>>,
    cancel: &'a CancellationToken,
    ring: &'static Ring<CompactionEvent>,
) -> Summarizer<'a> {
    Summarizer {
        provider,
        model: "m",
        max_output_tokens: Some(4096),
        options: ProviderOptions {
            reasoning_effort: Some("high"),
            fast: true,
        },
        reasoning_efforts,
        conversation,
        session_id: None,
        cancel,
        trace: Tracer::new(ring, CONTEXT),
        meter: Meter::new(Box::leak(Box::new(NetworkRing::new())), CONTEXT),
    }
}

#[test]
fn lowest_reasoning_effort_turns_reasoning_off_when_the_model_allows_it() {
    assert_eq!(
        lowest_reasoning_effort(&efforts(&["none", "minimal", "low", "xhigh"])),
        Some("none")
    );
    assert_eq!(
        lowest_reasoning_effort(&efforts(&["low", "medium", "xhigh", "max"])),
        Some("low")
    );
    assert_eq!(lowest_reasoning_effort(&[]), None);
}

#[tokio::test]
async fn a_summary_request_is_one_system_and_one_user_message_at_the_lowest_reasoning() {
    let provider = ScriptedProvider::new(vec![Ok(text("notes"))]);
    let levels = efforts(&["low", "high"]);
    let cancel = CancellationToken::new();
    let mut model = summarizer(&provider, &levels, None, &cancel);
    let reply = model
        .summarize(Prompt {
            system: "the compactor's instructions",
            user: "write the notes",
            after_conversation: false,
        })
        .await;
    assert_eq!(reply, Ok("notes".to_owned()));
    let seen = &provider.seen()[0];
    assert_eq!(seen.model, "m");
    assert_eq!(seen.instructions, ["the compactor's instructions"]);
    assert_eq!(seen.messages, [ChatMessage::user("write the notes")]);
    assert!(seen.tools.is_empty());
    assert_eq!(seen.tool_choice, ToolChoice::None);
    assert_eq!(seen.max_output_tokens, Some(4096));
    assert_eq!(seen.reasoning_effort.as_deref(), Some("low"));
    assert!(seen.fast);
}

#[tokio::test]
async fn a_request_after_the_conversation_sends_it_unchanged_then_the_request() {
    let provider = ScriptedProvider::new(vec![Ok(text("notes"))]);
    let levels = efforts(&["none", "high"]);
    let cancel = CancellationToken::new();
    let history = [
        ChatMessage::user("fix the build"),
        ChatMessage::Assistant {
            content: Some("Fixed.".to_owned()),
            tool_calls: Vec::new(),
            provider_replay: None,
        },
    ];
    let tools = [ToolSpec {
        name: "shell".to_owned(),
        description: String::new(),
        input_schema: "{}".into(),
    }];
    let conversation = ModelRequest {
        model: "m",
        instructions: &["the agent's instructions"],
        messages: &history,
        tools: &tools,
        tool_choice: ToolChoice::Auto,
        max_output_tokens: Some(4096),
        provider_options: ProviderOptions {
            reasoning_effort: Some("high"),
            fast: false,
        },
        session_id: None,
    };
    let mut model = summarizer(&provider, &levels, Some(conversation), &cancel);
    let reply = model
        .summarize(Prompt {
            system: "",
            user: "write the notes",
            after_conversation: true,
        })
        .await;
    assert_eq!(reply, Ok("notes".to_owned()));
    let seen = &provider.seen()[0];
    assert_eq!(seen.instructions, ["the agent's instructions"]);
    assert_eq!(seen.messages.len(), 3);
    assert_eq!(seen.messages[2], ChatMessage::user("write the notes"));
    assert_eq!(seen.tools, ["shell"]);
    assert_eq!(seen.tool_choice, ToolChoice::Auto);
    assert_eq!(seen.reasoning_effort.as_deref(), Some("high"));
    assert!(!seen.fast);

    let plain = model
        .summarize(Prompt {
            system: "s",
            user: "u",
            after_conversation: false,
        })
        .await;
    assert!(plain.is_err());
    assert_eq!(provider.seen()[1].reasoning_effort.as_deref(), Some("none"));
}

#[tokio::test]
async fn failures_name_why_the_summary_is_missing() {
    let provider = ScriptedProvider::new(vec![
        Err(failure(ProviderErrorKind::InvalidRequest, "BadRequest")),
        Err(failure(ProviderErrorKind::Protocol, "OutputTruncated")),
    ]);
    let cancel = CancellationToken::new();
    let mut model = summarizer(&provider, &[], None, &cancel);
    let prompt = Prompt {
        system: "s",
        user: "u",
        after_conversation: false,
    };
    assert_eq!(
        model.summarize(prompt).await,
        Err(CompactionError::ModelFailed)
    );
    assert_eq!(
        model.summarize(prompt).await,
        Err(CompactionError::SummaryIncomplete)
    );
    assert_eq!(provider.seen().len(), 2);
    assert_eq!(provider.seen()[0].reasoning_effort, None);
    cancel.cancel();
    assert_eq!(
        model.summarize(prompt).await,
        Err(CompactionError::Cancelled)
    );
}

#[tokio::test]
async fn each_summary_request_and_its_failure_reach_the_compaction_trace() {
    let usage = Usage {
        input_tokens: Some(1_200),
        output_tokens: Some(80),
    };
    let provider = ScriptedProvider::new(vec![
        Err(ProviderError {
            status: Some(400),
            ..failure(ProviderErrorKind::InvalidRequest, "BadRequest")
        }),
        Err(failure(ProviderErrorKind::Protocol, "OutputTruncated")),
        Ok(calling(ToolCall::new("c", "shell", "{}"))),
        Err(failure(ProviderErrorKind::Protocol, "InvalidFinishReason")),
        Ok(Completion {
            usage,
            ..text("notes")
        }),
    ]);
    let cancel = CancellationToken::new();
    let ring = ring();
    let mut model = traced_summarizer(&provider, &[], None, &cancel, ring);
    let prompt = Prompt {
        system: "s",
        user: "u",
        after_conversation: false,
    };
    for _ in 0..5 {
        let _ = model.summarize(prompt).await;
    }
    let call = |usage: &str| {
        (
            CompactionTraceKind::Log,
            false,
            format!("compaction model call model=m after_conversation=false {usage}"),
        )
    };
    let unknown =
        "input_tokens=null cache_read_tokens=null cache_write_tokens=null output_tokens=null";
    assert_eq!(
        traced(ring),
        [
            call(unknown),
            (
                CompactionTraceKind::SummaryTransportFailed,
                true,
                "model=m kind=invalid_request detail=".to_owned()
            ),
            call(unknown),
            (
                CompactionTraceKind::SummaryIncomplete,
                true,
                "model=m finish_reason=length bytes=0".to_owned()
            ),
            call(unknown),
            (
                CompactionTraceKind::SummaryToolCallRejected,
                true,
                "model=m".to_owned()
            ),
            call(unknown),
            (
                CompactionTraceKind::SummaryTransportFailed,
                true,
                "model=m err=InvalidFinishReason".to_owned()
            ),
            call(
                "input_tokens=1200 cache_read_tokens=null cache_write_tokens=null output_tokens=80"
            ),
        ]
    );
    let events = ring.snapshot();
    assert_eq!(events[1].event.context.turn_id, 4);
    assert_eq!(events[1].event.context.step_id, 9);
    assert_eq!(events[0].event.context, TraceContext::default());
    cancel.cancel();
    let _ = model.summarize(prompt).await;
    assert_eq!(ring.snapshot().len(), 9);
}
