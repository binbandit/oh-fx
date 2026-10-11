use ofx_contract::{ChatMessage, ProviderReplay, ReplaySource, RestoredHistory};

use super::*;

fn earlier_reply(provider: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::user("Please check the notes."),
        ChatMessage::Assistant {
            content: Some(String::new()),
            tool_calls: Vec::new(),
            provider_replay: Some(ProviderReplay {
                source: ReplaySource {
                    provider: provider.to_owned(),
                    model: "model-a".to_owned(),
                    binding: None,
                },
                parts_json: "not-json".to_owned(),
            }),
        },
    ]
}

#[tokio::test]
async fn chat_completions_send_an_empty_earlier_reply_without_projecting_its_replay() {
    for provider in ["local", "codex"] {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["Checked."]))]);
        let home = tempfile::tempdir().unwrap();
        let setup = agent_setup(&home, &server).await;
        let mut agent = setup.agent(false);
        agent.restore(RestoredHistory {
            checkpoint: None,
            compaction_count: 0,
            messages: earlier_reply(provider),
            turn_starts: vec![0],
        });
        let report = agent
            .run_turn("Check the notes.", &mut |_| {}, &CancellationToken::new())
            .await;
        assert_eq!(
            report.outcome,
            TurnOutcome::Completed,
            "{provider} {report:?}"
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 1, "{provider}");
        let body = requests[0].body_text();
        assert!(!body.contains("not-json"), "{provider} {body}");
        let conversation: Vec<Value> = requests[0].json()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] != "system")
            .cloned()
            .collect();
        assert_eq!(
            conversation,
            [
                json!({"role": "user", "content": "Please check the notes."}),
                json!({"role": "assistant", "content": ""}),
                json!({"role": "user", "content": "Check the notes."}),
            ],
            "{provider}"
        );
    }
}
