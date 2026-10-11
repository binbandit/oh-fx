use ofx_contract::{RestoredHistory, ToolResultStatus};

use super::compaction::{unmetered, windowed};
use super::*;

const STATE: &str = "prose and reasoning";
const PROJECTED: &str = "reasoning of prose and reasoning";

fn sourced(parts: &str, provider: &str, model: &str) -> ProviderReplay {
    ProviderReplay {
        source: ReplaySource {
            provider: provider.to_owned(),
            model: model.to_owned(),
            binding: None,
        },
        parts_json: parts.to_owned(),
    }
}

fn read_step(content: Option<&str>, replay: ProviderReplay) -> ChatMessage {
    ChatMessage::Assistant {
        content: content.map(str::to_owned),
        tool_calls: vec![echo_call("call_read", r#"{"path":"a"}"#)],
        provider_replay: Some(replay),
    }
}

fn read_result(output: &str) -> ChatMessage {
    ChatMessage::Tool {
        call_id: ToolCallId::new("call_read"),
        tool_name: "echo".to_owned(),
        content: output.to_owned(),
        status: ToolResultStatus::Success,
        images: Vec::new(),
    }
}

fn restore(agent: &mut Agent, messages: Vec<ChatMessage>, turn_starts: Vec<usize>) {
    agent.restore(RestoredHistory {
        checkpoint: None,
        compaction_count: 0,
        messages,
        turn_starts,
    });
}

fn reads_the_notes(content: Option<&str>, replay: ProviderReplay) -> Vec<ChatMessage> {
    vec![
        ChatMessage::user("Please read the notes."),
        read_step(content, replay),
        read_result("retained_result"),
    ]
}

#[tokio::test]
async fn an_empty_earlier_reply_is_sent_with_only_its_reasoning_and_kept_whole() {
    for content in [None, Some("")] {
        let provider = FakeProvider::new(vec![
            text_reply("The saved result is intact."),
            text_reply("Still intact."),
        ]);
        let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
        let restored = reads_the_notes(content, replay(STATE));
        restore(&mut agent, restored.clone(), vec![0]);
        for prompt in ["Check the notes.", "Check them again."] {
            let (report, _) = run(&mut agent, prompt).await;
            assert_eq!(report.outcome, TurnOutcome::Completed, "{content:?}");
        }
        let requests = provider.requests();
        assert_eq!(requests.len(), 2, "{content:?}");
        for request in &requests {
            assert_eq!(
                request.messages[1],
                read_step(content, replay(PROJECTED)),
                "{content:?}"
            );
            assert_eq!(request.messages[2], restored[2], "{content:?}");
        }
        assert_eq!(
            provider.projections(),
            [
                (STATE.to_owned(), vec!["call_read".to_owned()], false, true),
                (STATE.to_owned(), vec!["call_read".to_owned()], false, true)
            ],
            "{content:?}"
        );
        assert_eq!(agent.history[..3], restored[..], "{content:?}");
    }
}

#[tokio::test]
async fn only_empty_replies_from_the_requested_provider_and_model_are_projected() {
    let cases = [
        (Some("kept"), replay("invalid"), "invalid"),
        (Some(" "), replay("invalid"), "invalid"),
        (Some(""), replay(PROJECTED), PROJECTED),
        (
            Some(""),
            sourced("invalid", "codex", "test-model"),
            "invalid",
        ),
        (
            Some(""),
            sourced("invalid", "fake", "other-model"),
            "invalid",
        ),
    ];
    for (content, saved, sent) in cases {
        let provider = FakeProvider::new(vec![text_reply("The saved result is intact.")]);
        let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
        let restored = reads_the_notes(content, saved.clone());
        restore(&mut agent, restored.clone(), vec![0]);
        let (report, _) = run(&mut agent, "Check the notes.").await;
        assert_eq!(
            report.outcome,
            TurnOutcome::Completed,
            "{content:?} {saved:?}"
        );
        let requests = provider.requests();
        assert_eq!(requests.len(), 1, "{content:?} {saved:?}");
        assert_eq!(
            requests[0].messages[1],
            read_step(
                content,
                ProviderReplay {
                    parts_json: sent.to_owned(),
                    ..saved.clone()
                }
            ),
            "{content:?} {saved:?}"
        );
        assert_eq!(agent.history[..3], restored[..], "{content:?} {saved:?}");
    }
}

#[tokio::test]
async fn an_empty_earlier_reply_that_cannot_be_projected_fails_the_turn_before_any_request() {
    let provider = FakeProvider::new(vec![text_reply("The saved result is intact.")]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let restored = reads_the_notes(Some(""), replay("invalid"));
    restore(&mut agent, restored.clone(), vec![0]);
    let (report, _) = run(&mut agent, "Check the notes.").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(
        report.failure,
        Some(TurnFailure::Provider(failure(
            ProviderErrorKind::Protocol,
            "InvalidProviderState"
        )))
    );
    assert!(provider.requests().is_empty());
    assert_eq!(agent.history[..], restored[..]);
}

#[tokio::test]
async fn the_current_turn_s_own_empty_steps_are_left_to_the_turn() {
    let provider = FakeProvider::new(vec![
        with_replay(tool_reply(&[("call-1", "{}")]), STATE),
        text_reply("done"),
        text_reply("again done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert!(provider.projections().is_empty());
    let (report, _) = run(&mut agent, "again").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = provider.requests();
    assert_eq!(replays(&requests[1].messages), [Some(STATE)]);
    assert_eq!(replays(&requests[2].messages), [Some(PROJECTED), None]);
    assert_eq!(
        provider.projections(),
        [(STATE.to_owned(), vec!["call-1".to_owned()], false, true)]
    );
    assert_eq!(replays(&agent.history), [Some(STATE), None, None]);
}

#[tokio::test]
async fn compaction_projects_the_empty_replies_it_retains() {
    let older = format!("OLDER_HISTORY_SENTINEL {}", "o".repeat(150_000));
    let recent = format!("RECENT_RECOVERED_RESULT\n{}", "r".repeat(1_000));
    let provider = FakeProvider::new(vec![
        unmetered(text_reply(
            "Turn 1\nIn between: Earlier work established the project facts.",
        )),
        unmetered(text_reply("The saved read result remains available.")),
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    let restored = vec![
        ChatMessage::user("earlier work"),
        ChatMessage::Assistant {
            content: None,
            tool_calls: vec![echo_call("old_read", r#"{"path":"old"}"#)],
            provider_replay: None,
        },
        ChatMessage::Tool {
            call_id: ToolCallId::new("old_read"),
            tool_name: "echo".to_owned(),
            content: older,
            status: ToolResultStatus::Success,
            images: Vec::new(),
        },
        ChatMessage::Assistant {
            content: Some("earlier work done".to_owned()),
            tool_calls: Vec::new(),
            provider_replay: None,
        },
        ChatMessage::user("recent work"),
        read_step(Some(""), replay(STATE)),
        read_result(&recent),
    ];
    restore(&mut agent, restored.clone(), vec![0, 4]);
    let (report, _) = run(&mut agent, "next").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let notes = &requests[0].messages;
    assert_eq!(notes[2], restored[2]);
    assert_eq!(notes[5], read_step(Some(""), replay(PROJECTED)));
    let rebuilt = &requests[1].messages;
    let ChatMessage::User { content, .. } = &rebuilt[0] else {
        panic!("{rebuilt:?}");
    };
    assert!(
        content.starts_with("<compacted_conversation>\n"),
        "{content}"
    );
    assert!(!format!("{rebuilt:?}").contains("OLDER_HISTORY_SENTINEL"));
    assert_eq!(rebuilt[1], restored[4]);
    assert_eq!(rebuilt[2], read_step(Some(""), replay(PROJECTED)));
    assert_eq!(rebuilt[3], restored[6]);
    assert_eq!(rebuilt[4], ChatMessage::user("next"));
    assert_eq!(agent.history[1..4], restored[4..]);
    assert_eq!(
        provider.projections(),
        [
            (STATE.to_owned(), vec!["call_read".to_owned()], false, true),
            (STATE.to_owned(), vec!["call_read".to_owned()], false, true)
        ]
    );
}
