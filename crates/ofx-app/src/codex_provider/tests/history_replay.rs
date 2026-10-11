use ofx_contract::{
    ChatMessage, ProviderReplay, ReplaySource, RestoredHistory, ToolCall, ToolCallId,
};

use super::*;

const STEP_STATE: &str = r#"[{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"opaque-cipher"},{"type":"message","offset":0,"length":15,"phase":"commentary"}]"#;
const REASONING_ONLY: &str =
    r#"[{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"opaque-cipher"}]"#;

fn codex_replay(parts: &str, provider: &str, model: &str) -> ProviderReplay {
    ProviderReplay {
        source: ReplaySource {
            provider: provider.to_owned(),
            model: model.to_owned(),
            binding: None,
        },
        parts_json: parts.to_owned(),
    }
}

fn read_readme() -> ToolCall {
    ToolCall::new("call_1", "read_file", r#"{"path":"README.md"}"#)
}

fn earlier_read(content: Option<&str>, parts: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::user("Read README.md"),
        ChatMessage::Assistant {
            content: content.map(str::to_owned),
            tool_calls: vec![read_readme()],
            provider_replay: Some(codex_replay(parts, "codex", MODEL)),
        },
        ChatMessage::Tool {
            call_id: ToolCallId::new("call_1"),
            tool_name: "read_file".to_owned(),
            content: "1\t# Readme".to_owned(),
            status: ToolResultStatus::Success,
            images: Vec::new(),
        },
        ChatMessage::Assistant {
            content: Some("Done.".to_owned()),
            tool_calls: Vec::new(),
            provider_replay: None,
        },
    ]
}

fn restored(messages: Vec<ChatMessage>) -> RestoredHistory {
    RestoredHistory {
        checkpoint: None,
        compaction_count: 0,
        messages,
        turn_starts: vec![0],
    }
}

async fn codex_agent(fixture: &Fixture, codex: &FakeServer) -> Agent {
    let auth = FakeServer::start([]);
    let subscription = fixture
        .subscription(subscription_endpoints(&auth, codex))
        .await
        .expect("the saved login builds a subscription");
    let login = Arc::new(CodexLogin::default());
    login.sign_in(Arc::new(subscription));
    fixture.agent_on(
        Arc::new(SubscriptionProvider::new(login)),
        agent_config(MODEL, None, false),
    )
}

fn input(request: &RecordedRequest) -> Vec<Value> {
    request.json()["input"].as_array().expect("input").clone()
}

fn types(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|item| {
            item["type"]
                .as_str()
                .or_else(|| item["role"].as_str())
                .expect("item type")
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn an_empty_earlier_step_is_sent_with_its_reasoning_but_without_its_prose() {
    for content in [None, Some("")] {
        let fixture = Fixture::new();
        fixture.write_session(FAR_FUTURE_MS, 0o600);
        let codex = FakeServer::start([
            Reply::sse(&text_events("Checked.")),
            Reply::sse(&text_events("Checked again.")),
        ]);
        let mut agent = codex_agent(&fixture, &codex).await;
        agent.restore(restored(earlier_read(content, STEP_STATE)));
        for prompt in ["Check it.", "Check it again."] {
            let (report, _) = run(&mut agent, prompt).await;
            assert_eq!(
                report.outcome,
                TurnOutcome::Completed,
                "{content:?} {report:?}"
            );
        }
        let requests = codex.requests();
        assert_eq!(requests.len(), 2, "{content:?}");
        for request in &requests {
            let sent = input(request);
            let start = sent
                .iter()
                .position(|item| item["content"][0]["text"] == "Read README.md")
                .expect("the earlier prompt");
            let earlier = &sent[start..start + 5];
            assert_eq!(
                types(earlier),
                [
                    "user",
                    "reasoning",
                    "function_call",
                    "function_call_output",
                    "message"
                ],
                "{content:?} {sent:?}"
            );
            assert_eq!(earlier[1]["encrypted_content"], "opaque-cipher");
            assert_eq!(earlier[2]["call_id"], "call_1");
            assert_eq!(earlier[4]["content"][0]["text"], "Done.");
            assert!(!request.body_text().contains("commentary"), "{content:?}");
        }
    }
}

#[tokio::test]
async fn earlier_replies_keep_their_replay_unless_empty_and_from_the_requested_model() {
    const PROJECTION: Option<&str> = Some("InvalidProviderState");
    const REQUEST: Option<&str> = Some("InvalidOpenAICodexProviderState");
    let message = |length: usize| format!(r#"[{{"type":"message","offset":0,"length":{length}}}]"#);
    let unknown = r#"[{"type":"unknown"}]"#.to_owned();
    let cases = [
        ("", "{".to_owned(), "codex", MODEL, PROJECTION),
        ("", "[42]".to_owned(), "codex", MODEL, PROJECTION),
        ("", unknown, "codex", MODEL, PROJECTION),
        ("x", message(42), "codex", MODEL, REQUEST),
        (" ", message(42), "codex", MODEL, REQUEST),
        ("kept", message(4), "codex", MODEL, None),
        ("", REASONING_ONLY.to_owned(), "codex", MODEL, None),
        ("", "not-json".to_owned(), "gateway", MODEL, None),
        ("", "not-json".to_owned(), "codex", "other-model", None),
    ];
    for (content, parts, provider, model, refused) in cases {
        let fixture = Fixture::new();
        fixture.write_session(FAR_FUTURE_MS, 0o600);
        let codex = FakeServer::start([Reply::sse(&text_events("The saved result is intact."))]);
        let mut agent = codex_agent(&fixture, &codex).await;
        agent.restore(restored(vec![
            ChatMessage::user("Please check the notes."),
            ChatMessage::Assistant {
                content: Some(content.to_owned()),
                tool_calls: Vec::new(),
                provider_replay: Some(codex_replay(&parts, provider, model)),
            },
        ]));
        let (report, _) = run(&mut agent, "Check the notes.").await;
        let case = format!("{content:?} {parts} {provider} {model}");
        match refused {
            None => {
                assert_eq!(report.outcome, TurnOutcome::Completed, "{case} {report:?}");
                assert_eq!(codex.requests().len(), 1, "{case}");
            }
            Some(code) => {
                assert_eq!(report.outcome, TurnOutcome::Failed, "{case}");
                let Some(TurnFailure::Provider(error)) = &report.failure else {
                    panic!("{case} {report:?}");
                };
                assert_eq!(error.code, code, "{case}");
                assert!(codex.requests().is_empty(), "{case}");
            }
        }
    }
}
