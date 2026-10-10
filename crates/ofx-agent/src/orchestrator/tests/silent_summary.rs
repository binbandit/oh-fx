use ofx_contract::INTERRUPTED_TURN_CONTEXT;

use super::turn_log::{logged_turn, logging_agent};
use super::*;

const SAVED_STEPS: [&str; 3] = [
    r#""" replay=false calls=["call-1"] results=["call-1=echo {}:Success"]"#,
    r#""" replay=false calls=["call-2"] results=["call-2=echo {}:Success"]"#,
    r#""" replay=true calls=[] results=[]"#,
];

fn silent_turn(after: impl IntoIterator<Item = Script>) -> Arc<FakeProvider> {
    let mut scripts = vec![
        tool_reply(&[("call-1", "{}")]),
        tool_reply(&[("call-2", "{}")]),
        with_replay(text_reply(""), "silent"),
    ];
    scripts.extend(after);
    FakeProvider::new(scripts)
}

fn tool_steps() -> Vec<ChatMessage> {
    ["call-1", "call-2"]
        .into_iter()
        .flat_map(|id| {
            [
                ChatMessage::Assistant {
                    content: None,
                    tool_calls: vec![echo_call(id, "{}")],
                    provider_replay: None,
                },
                tool_message(id, "echo {}", ToolResultStatus::Success),
            ]
        })
        .collect()
}

fn reply(text: &str, parts: Option<&str>) -> ChatMessage {
    ChatMessage::Assistant {
        content: Some(text.to_owned()),
        tool_calls: Vec::new(),
        provider_replay: parts.map(replay),
    }
}

fn conversation(tail: impl IntoIterator<Item = ChatMessage>) -> Vec<ChatMessage> {
    let mut messages = vec![ChatMessage::user("go")];
    messages.extend(tool_steps());
    messages.push(reply("", Some("silent")));
    messages.extend(tail);
    messages
}

#[tokio::test]
async fn the_request_after_a_silent_tool_turn_leaves_out_its_summary_prompt() {
    let provider = silent_turn([text_reply("Summary."), text_reply("next answer")]);
    let (mut agent, entries) = logging_agent(&provider);
    assert_eq!(run(&mut agent, "go").await.0.final_text, "Summary.");
    let summarized = conversation([reply("Summary.", None)]);
    assert_eq!(agent.history, summarized);
    assert_eq!(
        *entries.lock().unwrap(),
        [logged_turn(
            "go",
            &SAVED_STEPS,
            r#"replied "Summary." replay=false"#
        )]
    );

    run(&mut agent, "next").await;
    let requests = provider.requests();
    assert_eq!(
        requests[3].messages.last(),
        Some(&ChatMessage::user(SUMMARIZE_PROMPT))
    );
    let mut expected = summarized;
    expected.push(ChatMessage::user("next"));
    assert_eq!(requests[4].messages, expected);
}

#[tokio::test]
async fn a_silent_tool_turn_without_provider_state_drops_its_summary_prompt() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        tool_reply(&[("call-2", "{}")]),
        text_reply(""),
        text_reply("Summary."),
        text_reply("next answer"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    run(&mut agent, "go").await;
    run(&mut agent, "next").await;
    let mut expected = vec![ChatMessage::user("go")];
    expected.extend(tool_steps());
    expected.extend([reply("Summary.", None), ChatMessage::user("next")]);
    assert_eq!(provider.requests()[4].messages, expected);
}

#[tokio::test]
async fn a_summary_request_that_fails_leaves_out_the_summary_prompt() {
    let provider = silent_turn([
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::Unauthorized, "unauthorized"),
        ),
        text_reply("next answer"),
    ]);
    let (mut agent, entries) = logging_agent(&provider);
    assert_eq!(run(&mut agent, "go").await.0.outcome, TurnOutcome::Failed);
    assert_eq!(
        *entries.lock().unwrap(),
        [logged_turn(
            "go",
            &SAVED_STEPS,
            r#"replied "" replay=false"#
        )]
    );
    run(&mut agent, "next").await;
    assert_eq!(
        provider.requests()[4].messages,
        conversation([ChatMessage::user("next")])
    );
}

#[tokio::test]
async fn an_interrupted_summary_request_closes_the_turn_without_its_summary_prompt() {
    let provider = silent_turn([text_reply("next answer")]);
    let (mut agent, entries) = logging_agent(&provider);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let mut usage_reports = 0;
    let report = agent
        .run_turn(
            "go",
            &mut |event| {
                if matches!(event, UiEvent::UsageReported { .. }) {
                    usage_reports += 1;
                    if usage_reports == 3 {
                        trigger.cancel();
                    }
                }
            },
            &cancel,
        )
        .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(agent.history, conversation([]));
    assert_eq!(
        *entries.lock().unwrap(),
        [logged_turn("go", &SAVED_STEPS, r#"Cancelled """#)]
    );
    run(&mut agent, "next").await;
    assert_eq!(
        provider.requests()[3].messages,
        conversation([
            ChatMessage::user(INTERRUPTED_TURN_CONTEXT),
            ChatMessage::user("next"),
        ])
    );
}
