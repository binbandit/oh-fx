use std::time::Duration;

use ofx_contract::{ChatMessage, ProviderError, ProviderOptions, ToolCall, ToolChoice, Usage};
use ofx_trace::{NetworkRing, TraceContext};

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

fn meter() -> Meter {
    Meter::new(
        Box::leak(Box::new(NetworkRing::new())),
        TraceContext::default(),
    )
}

fn answered(error: ProviderError, status: u16) -> ProviderError {
    ProviderError {
        status: Some(status),
        ..error
    }
}

fn replied(text: &str) -> Outcome {
    Outcome {
        reply: Ok(text.to_owned()),
        usage: Usage::default(),
    }
}

fn unusable(reason: Reason, detail: &str) -> Outcome {
    Outcome {
        reply: Err(Failure {
            reason,
            detail: detail.to_owned(),
        }),
        usage: Usage::default(),
    }
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
        complete(&provider, &request(&messages), 1024, meter(), &cancel).await,
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
        complete(&calls, &request(&messages), 1024, meter(), &cancel).await,
        Ok(unusable(Reason::ToolCall, ""))
    );
    let truncated = ScriptedProvider::new(vec![Err(failure(
        ProviderErrorKind::Protocol,
        "OutputTruncated",
    ))]);
    assert_eq!(
        complete(&truncated, &request(&messages), 1024, meter(), &cancel).await,
        Ok(unusable(Reason::Incomplete, "finish_reason=length bytes=0"))
    );
    let unfinished = ScriptedProvider::new(vec![Ok(Completion {
        finish_reason: FinishReason::ToolCalls,
        ..text("partial")
    })]);
    assert_eq!(
        complete(&unfinished, &request(&messages), 1024, meter(), &cancel).await,
        Ok(unusable(
            Reason::Incomplete,
            "finish_reason=tool_calls bytes=7"
        ))
    );
    let oversized = ScriptedProvider::new(vec![Ok(text("0123456789"))]);
    assert_eq!(
        complete(&oversized, &request(&messages), 9, meter(), &cancel).await,
        Ok(unusable(Reason::Truncated, "bytes=10"))
    );
    let refused = ScriptedProvider::new(vec![Err(answered(
        failure(ProviderErrorKind::InvalidRequest, "BadRequest"),
        400,
    ))]);
    assert_eq!(
        complete(&refused, &request(&messages), 1024, meter(), &cancel).await,
        Ok(unusable(Reason::Provider, "kind=invalid_request detail="))
    );
    assert_eq!(refused.seen().len(), 1);
    let broken = ScriptedProvider::new(vec![Err(failure(
        ProviderErrorKind::Protocol,
        "InvalidFinishReason",
    ))]);
    assert_eq!(
        complete(&broken, &request(&messages), 1024, meter(), &cancel).await,
        Ok(unusable(Reason::Transport, "err=InvalidFinishReason"))
    );
    let filtered = ScriptedProvider::new(vec![Err(failure(
        ProviderErrorKind::ProviderError,
        "ContentFiltered",
    ))]);
    assert_eq!(
        complete(&filtered, &request(&messages), 1024, meter(), &cancel).await,
        Ok(unusable(Reason::Transport, "err=ContentFiltered"))
    );
}

#[tokio::test]
async fn a_provider_error_detail_is_masked_before_its_preview_is_cut() {
    let forms = [
        ("https://user:", "hunter2secretvalue", "@example.com/path"),
        (
            "Authorization: Bearer ",
            "abcdefghijklmnopqrstuvwxyz0123456789",
            " end",
        ),
        ("API_KEY=", "supersecretvalue1234", " next"),
        ("{\"api_key\":\"", "supersecretvalue1234", "\"}"),
    ];
    let messages = [ChatMessage::user("write them")];
    let cancel = CancellationToken::new();
    for (before, secret, after) in forms {
        for kept in 0..=secret.len() {
            let padding = "x".repeat(DETAIL_PREVIEW_BYTES - before.len() - kept);
            let rejected = ScriptedProvider::new(vec![Err(answered(
                failure(ProviderErrorKind::Unauthorized, "Unauthorized"),
                401,
            )
            .with_detail(format!("{padding}{before}{secret}{after}")))]);
            let Ok(Outcome {
                reply: Err(rejection),
                ..
            }) = complete(&rejected, &request(&messages), 1024, meter(), &cancel).await
            else {
                panic!("the request should fail");
            };
            assert!(
                !rejection.detail.contains(&secret[..4]),
                "{before}{secret} cut after {kept}: {}",
                rejection.detail
            );
        }
    }
}

#[tokio::test]
async fn a_provider_error_detail_is_masked_and_kept_to_one_safe_line() {
    let messages = [ChatMessage::user("write them")];
    let cancel = CancellationToken::new();
    let detail = format!(
        "  bad key Bearer abcdefghijklmnop \x1b[31mred\x1b[0m\tdone {}\nsecond line",
        "x".repeat(300)
    );
    let rejected = ScriptedProvider::new(vec![Err(answered(
        failure(ProviderErrorKind::Unauthorized, "Unauthorized"),
        401,
    )
    .with_detail(detail))]);
    let Ok(Outcome {
        reply: Err(rejection),
        ..
    }) = complete(&rejected, &request(&messages), 1024, meter(), &cancel).await
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
        complete(&unmapped, &request(&messages), 1024, meter(), &cancel).await,
        Ok(unusable(Reason::Provider, "kind=provider_error detail="))
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
        complete(&provider, &request(&messages), 1024, meter(), &cancel).await,
        Ok(replied("the notes"))
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
        complete(&provider, &request(&messages), 1024, meter(), &cancel).await,
        Err(Cancelled)
    );
    assert!(provider.seen().is_empty());
    let interrupted = ScriptedProvider::new(vec![Err(ProviderError::cancelled())]);
    assert_eq!(
        complete(
            &interrupted,
            &request(&messages),
            1024,
            meter(),
            &CancellationToken::new()
        )
        .await,
        Err(Cancelled)
    );
}

#[tokio::test(start_paused = true)]
async fn each_request_reaches_the_network_ring_with_the_callers_ids() {
    let ring: &'static NetworkRing = Box::leak(Box::new(NetworkRing::new()));
    let context = TraceContext {
        turn_id: 3,
        step_id: 5,
        subagent_id: 0,
    };
    let provider = ScriptedProvider::new(vec![
        Err(failure(
            ProviderErrorKind::TransportInterrupted,
            "ReadFailed",
        )),
        Ok(Completion {
            usage: Usage {
                input_tokens: Some(70),
                output_tokens: Some(9),
            },
            ..text("the notes")
        }),
    ]);
    let messages = [ChatMessage::user("write them")];
    let outcome = complete(
        &provider,
        &request(&messages),
        1024,
        Meter::new(ring, context),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(
        outcome.map(|outcome| outcome.reply),
        Ok(Ok("the notes".to_owned()))
    );
    let calls = ring.snapshot().calls;
    let shown: Vec<_> = calls
        .iter()
        .map(|call| {
            (
                call.model.as_str(),
                call.status,
                call.error.as_str(),
                call.stop_reason.as_str(),
                call.response_bytes,
                (call.input_tokens, call.output_tokens),
                (call.turn_id, call.step_id),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            ("m", 0, "ReadFailed", "", 0, (0, 0), (3, 5)),
            ("m", 200, "", "stop", 9, (70, 9), (3, 5)),
        ]
    );
}
