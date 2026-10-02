use std::time::Duration;

use ofx_contract::{ChatMessage, ProviderError, ProviderOptions, ToolCall, ToolCallId, ToolChoice};

use super::*;
use crate::scripted_provider::{ScriptedProvider, calling, failure, text};

fn request(messages: &[ChatMessage]) -> ModelRequest<'_> {
    ModelRequest {
        model: "m",
        instructions: &["system"],
        messages,
        tools: &[],
        tool_choice: ToolChoice::None,
        max_output_tokens: None,
        provider_options: ProviderOptions::default(),
        session_id: None,
    }
}

#[tokio::test]
async fn a_complete_reply_is_returned_whole() {
    let provider = ScriptedProvider::new(vec![Ok(text("the notes"))]);
    let messages = [ChatMessage::user("write them")];
    let cancel = CancellationToken::new();
    assert_eq!(
        complete(&provider, &request(&messages), 1024, &cancel).await,
        Ok("the notes".to_owned())
    );
    assert_eq!(provider.seen().len(), 1);
}

#[tokio::test]
async fn a_tool_call_a_truncated_or_an_oversized_reply_is_not_used() {
    let messages = [ChatMessage::user("write them")];
    let cancel = CancellationToken::new();
    let call = ToolCall {
        id: ToolCallId::new("c"),
        name: "shell".to_owned(),
        arguments: "{}".to_owned(),
    };
    let calls = ScriptedProvider::new(vec![Ok(calling(call))]);
    assert_eq!(
        complete(&calls, &request(&messages), 1024, &cancel).await,
        Err(Failure::Unusable)
    );
    let truncated = ScriptedProvider::new(vec![Err(failure(
        ProviderErrorKind::Protocol,
        "OutputTruncated",
    ))]);
    assert_eq!(
        complete(&truncated, &request(&messages), 1024, &cancel).await,
        Err(Failure::Incomplete)
    );
    let oversized = ScriptedProvider::new(vec![Ok(text("0123456789"))]);
    assert_eq!(
        complete(&oversized, &request(&messages), 9, &cancel).await,
        Err(Failure::Unusable)
    );
    let refused = ScriptedProvider::new(vec![Err(failure(
        ProviderErrorKind::InvalidRequest,
        "BadRequest",
    ))]);
    assert_eq!(
        complete(&refused, &request(&messages), 1024, &cancel).await,
        Err(Failure::Unusable)
    );
    assert_eq!(refused.seen().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn transient_failures_are_retried_before_anything_streams() {
    let provider = ScriptedProvider::new(vec![
        Err(ProviderError::new(
            ProviderErrorKind::ServerError,
            "ServerError",
        )),
        Err(ProviderError {
            retry_after: Some(Duration::from_secs(3)),
            ..ProviderError::new(ProviderErrorKind::RateLimited, "RateLimited")
        }),
        Ok(text("the notes")),
    ]);
    let messages = [ChatMessage::user("write them")];
    let cancel = CancellationToken::new();
    let started = tokio::time::Instant::now();
    assert_eq!(
        complete(&provider, &request(&messages), 1024, &cancel).await,
        Ok("the notes".to_owned())
    );
    assert_eq!(provider.seen().len(), 3);
    assert_eq!(started.elapsed(), Duration::from_millis(3_250));
}

#[tokio::test]
async fn cancellation_stops_the_request() {
    let provider = ScriptedProvider::new(vec![Ok(text("late"))]);
    let messages = [ChatMessage::user("write them")];
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        complete(&provider, &request(&messages), 1024, &cancel).await,
        Err(Failure::Cancelled)
    );
    assert!(provider.seen().is_empty());
    let interrupted = ScriptedProvider::new(vec![Err(ProviderError::cancelled())]);
    assert_eq!(
        complete(
            &interrupted,
            &request(&messages),
            1024,
            &CancellationToken::new()
        )
        .await,
        Err(Failure::Cancelled)
    );
}
