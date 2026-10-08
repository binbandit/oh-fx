use ofx_contract::{INTERRUPTED_BEFORE_COMPLETION, INTERRUPTED_TURN_CONTEXT};

use super::*;

fn assistant(text: &str) -> ChatMessage {
    ChatMessage::Assistant {
        content: Some(text.to_owned()),
        tool_calls: Vec::new(),
        provider_replay: None,
    }
}

#[tokio::test]
async fn live_cancelled_turns_close_once_before_an_ordinary_follow_up() {
    for partial in ["", "half"] {
        let mut scripts = Vec::new();
        if !partial.is_empty() {
            scripts.push(Script::StreamThenWait(vec![StreamEvent::TextDelta {
                text: partial.to_owned(),
            }]));
        }
        scripts.extend([text_reply("next answer"), text_reply("third answer")]);
        let provider = FakeProvider::new(scripts);
        let mut agent = new_agent(Arc::clone(&provider), Vec::new());
        let cancel = CancellationToken::new();
        if partial.is_empty() {
            cancel.cancel();
        }
        let report = agent
            .run_turn(
                "first",
                &mut |event| {
                    if matches!(event, UiEvent::AssistantText { .. }) {
                        cancel.cancel();
                    }
                },
                &cancel,
            )
            .await;
        assert_eq!(report.outcome, TurnOutcome::Interrupted);
        run(&mut agent, "next").await;
        let closed = if partial.is_empty() {
            INTERRUPTED_BEFORE_COMPLETION.to_owned()
        } else {
            format!("{partial}\n\n{INTERRUPTED_BEFORE_COMPLETION}")
        };
        let requests = provider.requests();
        let next = requests.last().unwrap();
        assert_eq!(
            next.messages,
            [
                ChatMessage::user("first"),
                assistant(&closed),
                ChatMessage::user(INTERRUPTED_TURN_CONTEXT),
                ChatMessage::user("next")
            ]
        );
        run(&mut agent, "third").await;
        let requests = provider.requests();
        let third = requests.last().unwrap();
        assert_eq!(
            third
                .messages
                .iter()
                .filter(|message| **message == ChatMessage::user(INTERRUPTED_TURN_CONTEXT))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn a_failed_partial_reply_closes_before_the_next_request() {
    let provider = FakeProvider::new(vec![
        Script::Fail(
            vec![StreamEvent::TextDelta {
                text: "half".to_owned(),
            }],
            ProviderError::new(ProviderErrorKind::Protocol, "BadData"),
        ),
        text_reply("next answer"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, _) = run(&mut agent, "first").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    run(&mut agent, "next").await;
    assert_eq!(
        provider.requests()[1].messages,
        [
            ChatMessage::user("first"),
            assistant(&format!("half\n\n{INTERRUPTED_BEFORE_COMPLETION}")),
            ChatMessage::user(INTERRUPTED_TURN_CONTEXT),
            ChatMessage::user("next")
        ]
    );
}

#[tokio::test]
async fn clearing_an_interrupted_turn_drops_its_pending_closure() {
    let provider = FakeProvider::new(vec![text_reply("next answer")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let cancel = CancellationToken::new();
    cancel.cancel();
    agent.run_turn("first", &mut |_| {}, &cancel).await;
    agent.clear_history();
    run(&mut agent, "next").await;
    assert_eq!(provider.requests()[0].messages, [ChatMessage::user("next")]);
}

#[tokio::test]
async fn restoring_history_replaces_the_pending_live_interruption() {
    let provider = FakeProvider::new(vec![text_reply("next answer")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let cancel = CancellationToken::new();
    cancel.cancel();
    agent.run_turn("first", &mut |_| {}, &cancel).await;
    agent.restore(ofx_contract::RestoredHistory {
        checkpoint: None,
        messages: vec![ChatMessage::user("restored"), assistant("done")],
        turn_starts: vec![0],
    });
    run(&mut agent, "next").await;
    assert_eq!(
        provider.requests()[0].messages,
        [
            ChatMessage::user("restored"),
            assistant("done"),
            ChatMessage::user("next")
        ]
    );
}
