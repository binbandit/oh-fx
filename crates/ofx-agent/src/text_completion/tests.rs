use std::time::Duration;

use ofx_contract::{ChatMessage, ProviderError, ProviderOptions, ToolCall, ToolChoice, Usage};

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

fn answered(text: &str) -> Result<Outcome, Cancelled> {
    Ok(Outcome {
        reply: Ok(text.to_owned()),
        usage: Usage::default(),
    })
}

fn unusable(reason: Reason, detail: &str) -> Result<Outcome, Cancelled> {
    Ok(Outcome {
        reply: Err(Failure {
            reason,
            detail: detail.to_owned(),
        }),
        usage: Usage::default(),
    })
}

#[tokio::test]
async fn a_complete_reply_is_returned_whole_with_its_usage() {
    let usage = Usage {
        input_tokens: Some(1_200),
        output_tokens: Some(80),
    };
    let provider = ScriptedProvider::new(vec![Ok(Completion {
        usage,
        ..text("the notes")
    })]);
    let messages = [ChatMessage::user("write them")];
    let cancel = CancellationToken::new();
    assert_eq!(
        complete(&provider, &request(&messages), 1024, &cancel).await,
        Ok(Outcome {
            reply: Ok("the notes".to_owned()),
            usage,
        })
    );
    assert_eq!(provider.seen().len(), 1);
}

#[tokio::test]
async fn a_tool_call_a_truncated_or_an_oversized_reply_is_not_used() {
    let messages = [ChatMessage::user("write them")];
    let cancel = CancellationToken::new();
    let call = ToolCall::new("c", "shell", "{}");
    let calls = ScriptedProvider::new(vec![Ok(calling(call))]);
    assert_eq!(
        complete(&calls, &request(&messages), 1024, &cancel).await,
        unusable(Reason::ToolCall, "")
    );
    let truncated = ScriptedProvider::new(vec![Err(failure(
        ProviderErrorKind::Protocol,
        "OutputTruncated",
    ))]);
    assert_eq!(
        complete(&truncated, &request(&messages), 1024, &cancel).await,
        unusable(Reason::Incomplete, "finish_reason=length bytes=0")
    );
    let unfinished = ScriptedProvider::new(vec![Ok(Completion {
        finish_reason: FinishReason::ToolCalls,
        ..text("partial")
    })]);
    assert_eq!(
        complete(&unfinished, &request(&messages), 1024, &cancel).await,
        unusable(Reason::Incomplete, "finish_reason=tool_calls bytes=7")
    );
    let oversized = ScriptedProvider::new(vec![Ok(text("0123456789"))]);
    assert_eq!(
        complete(&oversized, &request(&messages), 9, &cancel).await,
        unusable(Reason::Truncated, "bytes=10")
    );
    let refused = ScriptedProvider::new(vec![Err(failure(
        ProviderErrorKind::InvalidRequest,
        "BadRequest",
    ))]);
    assert_eq!(
        complete(&refused, &request(&messages), 1024, &cancel).await,
        unusable(Reason::Provider, "kind=invalid_request detail=")
    );
    assert_eq!(refused.seen().len(), 1);
    let broken = ScriptedProvider::new(vec![Err(failure(
        ProviderErrorKind::Protocol,
        "InvalidFinishReason",
    ))]);
    assert_eq!(
        complete(&broken, &request(&messages), 1024, &cancel).await,
        unusable(Reason::Transport, "err=InvalidFinishReason")
    );
}

#[tokio::test]
async fn a_provider_error_detail_is_masked_and_kept_to_one_safe_line() {
    let messages = [ChatMessage::user("write them")];
    let cancel = CancellationToken::new();
    let detail = format!(
        "  bad key Bearer abcdefghijklmnop \x1b[31mred\x1b[0m\tdone {}\nsecond line",
        "x".repeat(300)
    );
    let rejected = ScriptedProvider::new(vec![Err(failure(
        ProviderErrorKind::Unauthorized,
        "Unauthorized",
    )
    .with_detail(detail))]);
    let Ok(Outcome {
        reply: Err(rejection),
        ..
    }) = complete(&rejected, &request(&messages), 1024, &cancel).await
    else {
        panic!("the request should fail");
    };
    assert_eq!(rejection.reason, Reason::Provider);
    let shown = rejection
        .detail
        .strip_prefix("kind=unauthorized detail=")
        .expect("the detail names its kind");
    assert!(
        shown.starts_with("bad key [redacted] red done xxx"),
        "{shown}"
    );
    assert_eq!(shown.len(), 240);
    let unmapped = ScriptedProvider::new(vec![Err(ProviderError {
        status: Some(418),
        ..failure(ProviderErrorKind::ProviderError, "ProviderError")
    })]);
    assert_eq!(
        complete(&unmapped, &request(&messages), 1024, &cancel).await,
        unusable(Reason::Provider, "kind=provider_error detail=")
    );
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
        answered("the notes")
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
        Err(Cancelled)
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
        Err(Cancelled)
    );
}
