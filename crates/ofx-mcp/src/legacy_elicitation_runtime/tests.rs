use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use ofx_contract::{BoxFuture, QuestionAsker, QuestionBatchEntry};

use super::*;

struct Answers(Mutex<Vec<&'static str>>);

impl QuestionAsker for Answers {
    fn ask(&self, _entries: Vec<QuestionBatchEntry>) -> BoxFuture<'static, Option<Vec<String>>> {
        let answer = self.0.lock().unwrap().pop();
        Box::pin(async move { answer.map(|answer| vec![answer.to_owned()]) })
    }
}

struct Owner(Arc<AtomicBool>);

impl ElicitationOwner for Owner {
    fn current(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

fn answering(
    wire: ElicitationWire,
    answers: &[&'static str],
) -> (ElicitationContext, Arc<AtomicBool>) {
    let current = Arc::new(AtomicBool::new(true));
    let answers = answers.iter().rev().copied().collect();
    let context = ElicitationContext::new(
        wire,
        OperationElicitation {
            responder: InputResponder::new(Arc::new(Answers(Mutex::new(answers)))),
            server_name: "fixture".to_owned(),
            owner: Box::new(Owner(Arc::clone(&current))),
        },
        Instant::now() + Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .unwrap();
    (context, current)
}

const NAME_FORM: &str = r#"{"jsonrpc":"2.0","id":7,"method":"elicitation/create","params":{"message":"Who?","requestedSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}}}"#;

#[tokio::test]
async fn an_answered_form_is_returned_as_the_request_result() {
    let (context, _) = answering(ElicitationWire::LegacyMcp2025_11, &["Ada", "Submit"]);
    assert_eq!(
        context.respond(NAME_FORM.as_bytes()).await.unwrap(),
        r#"{"jsonrpc":"2.0","id":7,"result":{"action":"accept","content":{"name":"Ada"}}}"#
    );
    let (declined, _) = answering(ElicitationWire::LegacyMcp2025_06, &["Ada", "Decline"]);
    let string_id = NAME_FORM.replace("\"id\":7", "\"id\":\"e-1\"");
    assert_eq!(
        declined.respond(string_id.as_bytes()).await.unwrap(),
        r#"{"jsonrpc":"2.0","id":"e-1","result":{"action":"decline"}}"#
    );
    let (cancelled, _) = answering(ElicitationWire::LegacyMcp2025_11, &[]);
    assert_eq!(
        cancelled.respond(NAME_FORM.as_bytes()).await.unwrap(),
        r#"{"jsonrpc":"2.0","id":7,"result":{"action":"cancel"}}"#
    );
}

#[tokio::test]
async fn unsupported_client_features_answer_method_not_found() {
    let (context, _) = answering(ElicitationWire::LegacyMcp2025_11, &[]);
    for method in ["roots/list", "sampling/createMessage", "ping"] {
        let frame =
            format!("{{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"{method}\",\"params\":{{}}}}");
        assert_eq!(
            context.respond(frame.as_bytes()).await.unwrap(),
            "{\"jsonrpc\":\"2.0\",\"id\":9,\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}"
        );
        assert_eq!(
            method_not_found_response(frame.as_bytes()),
            "{\"jsonrpc\":\"2.0\",\"id\":9,\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}"
        );
    }
}

#[tokio::test]
async fn requests_are_refused_in_upstream_order_and_bounded() {
    let (context, _) = answering(ElicitationWire::LegacyMcp2025_11, &[]);
    let url = br#"{"jsonrpc":"2.0","id":2,"method":"elicitation/create","params":{"mode":"url","message":"Open","url":"https://example.test/","elicitationId":"e"}}"#;
    assert_eq!(
        context.respond(url).await.unwrap(),
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32602,\"message\":\"Unsupported elicitation mode\"}}"
    );
    for params in [
        r#"{"message":"hi"}"#,
        r#"{"message":"hi","requestedSchema":{"type":"object","properties":{"api_key":{"type":"string"}}}}"#,
        r#"{"message":"hi","requestedSchema":{"type":"object","properties":{"value":{"type":"string","pattern":"(?=x)"}}}}"#,
        r#"{"mode":"url","message":"Open","url":"http://example.test/","elicitationId":"e"}"#,
        "[]",
    ] {
        let frame = format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"elicitation/create","params":{params}}}"#
        );
        assert_eq!(
            context.respond(frame.as_bytes()).await.unwrap(),
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"error\":{\"code\":-32602,\"message\":\"Invalid params\"}}",
            "{params}"
        );
    }
    let missing = br#"{"jsonrpc":"2.0","id":1,"method":"elicitation/create"}"#;
    assert!(
        context
            .respond(missing)
            .await
            .unwrap()
            .contains("Invalid params")
    );
    let bad_id = br#"{"jsonrpc":"2.0","id":{},"method":"elicitation/create"}"#;
    assert_eq!(
        context.respond(bad_id).await.unwrap(),
        "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32600,\"message\":\"Invalid request id\"}}"
    );
    assert_eq!(
        context.respond(b"not json").await,
        Err(McpError::McpInvalidJson)
    );
    for _ in 0..25 {
        context.respond(missing).await.unwrap();
    }
    assert!(
        context
            .respond(missing)
            .await
            .unwrap()
            .contains("Elicitation request limit exceeded")
    );
    assert_eq!(
        server_request_failed_response(b"not json"),
        "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"Server request failed\"}}"
    );
    let legacy = context_without_answers(ElicitationWire::LegacyMcp2025_06);
    let explicit = br#"{"jsonrpc":"2.0","id":4,"method":"elicitation/create","params":{"mode":"form","message":"hi","requestedSchema":{"type":"object","properties":{}}}}"#;
    assert!(
        legacy
            .respond(explicit)
            .await
            .unwrap()
            .contains("Invalid params")
    );
}

fn context_without_answers(wire: ElicitationWire) -> ElicitationContext {
    answering(wire, &[]).0
}

#[tokio::test]
async fn a_changed_owner_or_a_failed_interaction_is_reported_by_name() {
    let (context, current) = answering(ElicitationWire::LegacyMcp2025_11, &["Ada", "Submit"]);
    current.store(false, Ordering::Release);
    assert_eq!(
        context.respond(NAME_FORM.as_bytes()).await.unwrap(),
        "{\"jsonrpc\":\"2.0\",\"id\":7,\"error\":{\"code\":-32603,\"message\":\"Elicitation owner changed\"}}"
    );
    let mut answers = vec!["Ada"];
    answers.extend(std::iter::repeat_n(["Other", "Keep current"], 8).flatten());
    let (looping, _) = answering(ElicitationWire::LegacyMcp2025_11, &answers);
    assert_eq!(
        looping.respond(NAME_FORM.as_bytes()).await.unwrap(),
        "{\"jsonrpc\":\"2.0\",\"id\":7,\"error\":{\"code\":-32603,\"message\":\"RetryLimitExceeded\"}}"
    );
}

#[tokio::test]
async fn a_finished_operation_cancels_its_open_question() {
    struct Silent;
    impl QuestionAsker for Silent {
        fn ask(
            &self,
            _entries: Vec<QuestionBatchEntry>,
        ) -> BoxFuture<'static, Option<Vec<String>>> {
            Box::pin(std::future::pending())
        }
    }
    let context = ElicitationContext::new(
        ElicitationWire::LegacyMcp2025_11,
        OperationElicitation {
            responder: InputResponder::new(Arc::new(Silent)),
            server_name: "fixture".to_owned(),
            owner: Box::new(Owner(Arc::new(AtomicBool::new(true)))),
        },
        Instant::now() + Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .unwrap();
    context.finish();
    assert_eq!(
        context.respond(NAME_FORM.as_bytes()).await.unwrap(),
        r#"{"jsonrpc":"2.0","id":7,"result":{"action":"cancel"}}"#
    );
}

#[tokio::test]
async fn an_operation_waits_until_every_open_answer_is_written() {
    let context = Arc::new(answering(ElicitationWire::LegacyMcp2025_11, &[]).0);
    let first = context.begin_answer();
    let second = context.begin_answer();
    let waiting = context.answered();
    tokio::pin!(waiting);
    assert!(futures_poll(&mut waiting).await.is_none());
    drop(first);
    assert!(futures_poll(&mut waiting).await.is_none());
    drop(second);
    waiting.await;
    context.answered().await;
}

async fn futures_poll<F: Future + Unpin>(future: &mut F) -> Option<F::Output> {
    std::future::poll_fn(|context| {
        std::task::Poll::Ready(match std::pin::Pin::new(&mut *future).poll(context) {
            std::task::Poll::Ready(output) => Some(output),
            std::task::Poll::Pending => None,
        })
    })
    .await
}
