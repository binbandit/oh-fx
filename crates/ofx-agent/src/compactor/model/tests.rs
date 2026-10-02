use ofx_contract::{ProviderErrorKind, ToolSpec};

use super::*;
use crate::scripted_provider::{ScriptedProvider, failure, text};

fn efforts(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn summarizer<'a>(
    provider: &'a ScriptedProvider,
    reasoning_efforts: &'a [String],
    conversation: Option<ModelRequest<'a>>,
    cancel: &'a CancellationToken,
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
        cancel,
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
        input_schema: "{}",
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
