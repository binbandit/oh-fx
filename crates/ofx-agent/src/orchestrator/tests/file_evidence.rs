use ofx_contract::{
    FileEvidence, FileEvidenceAction, RecoveredTurn, RecoveryProgress, RecoveryStrategy, ToolCall,
};

use super::compaction::{unmetered, windowed_with_tools};
use super::turn_log::{Logged, MemoryLog, logged};
use super::*;

fn file_tool(name: &str) -> Arc<dyn Tool> {
    Arc::new(EchoTool {
        spec: ToolSpec {
            name: name.to_owned(),
            description: "Work with files.".to_owned(),
            input_schema: r#"{"type":"object"}"#.into(),
        },
        cleaned_up: Arc::new(AtomicBool::new(false)),
        meeting: Arc::new(tokio::sync::Barrier::new(2)),
    })
}

fn file_calls(calls: &[(&str, &str, &str)]) -> Script {
    let calls = calls
        .iter()
        .map(|(id, name, arguments)| ToolCall::new(*id, *name, *arguments))
        .collect();
    Script::Reply(Vec::new(), completion(None, calls, FinishReason::ToolCalls))
}

fn file_agent(provider: &Arc<FakeProvider>) -> Agent {
    let shared: Arc<FakeProvider> = Arc::clone(provider);
    new_agent(
        shared,
        vec![file_tool("read_file"), file_tool("write_file")],
    )
}

const EVIDENCE: &str = "Session file evidence from previous tool execution. Re-read stale paths before relying on exact contents:\n\
    - action=read status=success path=a.rs model_view=full stale=true tool=read_file\n\
    - action=write status=success path=a.rs tool=write_file";

#[tokio::test]
async fn a_finished_turn_sends_its_file_evidence_after_its_tool_steps() {
    let provider = FakeProvider::new(vec![
        file_calls(&[("call-1", "read_file", r#"{"path":"a.rs","whole":1}"#)]),
        file_calls(&[("call-2", "write_file", r#"{"path":"a.rs"}"#)]),
        text_reply("rewrote it"),
        text_reply("next answer"),
    ]);
    let mut agent = file_agent(&provider);
    let (first, _) = run(&mut agent, "rewrite a.rs").await;
    assert_eq!(first.outcome, TurnOutcome::Completed);
    run(&mut agent, "and now?").await;
    let requests = provider.requests();
    let messages = &requests[3].messages;
    let evidence = messages
        .iter()
        .position(
            |message| matches!(message, ChatMessage::User { content, .. } if content == EVIDENCE),
        )
        .expect("the evidence message");
    assert!(
        matches!(&messages[evidence - 1], ChatMessage::Tool { call_id, .. } if call_id.as_str() == "call-2")
    );
    assert!(
        matches!(&messages[evidence + 1], ChatMessage::Assistant { content: Some(text), .. } if text == "rewrote it")
    );
    assert_eq!(messages.last(), Some(&ChatMessage::user("and now?")));
    assert!(
        !requests[2]
            .messages
            .iter()
            .any(|message| matches!(message, ChatMessage::User { content, .. } if content.starts_with("Session file evidence")))
    );
}

#[tokio::test]
async fn a_turn_without_file_tools_sends_no_evidence() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        text_reply("done"),
        text_reply("next"),
    ]);
    let mut agent = new_agent(
        Arc::clone(&provider) as Arc<FakeProvider>,
        vec![echo_tool()],
    );
    run(&mut agent, "echo").await;
    run(&mut agent, "again").await;
    let requests = provider.requests();
    assert!(
        !requests[2]
            .messages
            .iter()
            .any(|message| matches!(message, ChatMessage::User { content, .. } if content.starts_with("Session file evidence")))
    );
}

#[tokio::test]
async fn the_saved_turn_carries_the_same_evidence() {
    let provider = FakeProvider::new(vec![
        file_calls(&[("call-1", "read_file", r#"{"path":"a.rs","whole":1}"#)]),
        file_calls(&[("call-2", "write_file", r#"{"path":"a.rs"}"#)]),
        text_reply("rewrote it"),
    ]);
    let (log, entries) = MemoryLog::shared();
    let mut agent = logged(file_agent(&provider), log);
    run(&mut agent, "rewrite a.rs").await;
    let entries = entries.lock().unwrap().clone();
    let Logged::Turn { files, .. } = &entries[0] else {
        panic!("{entries:?}");
    };
    assert_eq!(
        files,
        &["read a.rs full stale".to_owned(), "write a.rs".to_owned(),]
    );
}

#[tokio::test]
async fn evidence_from_steps_a_compaction_cut_reaches_the_saved_turn_and_later_requests() {
    let big_step = format!("STEP_SENTINEL {}", "h".repeat(150_000));
    let provider = FakeProvider::new(vec![
        unmetered(Script::Reply(
            Vec::new(),
            completion(
                Some(&big_step),
                vec![ToolCall::new(
                    "call-1",
                    "read_file",
                    r#"{"path":"a.rs","whole":1}"#,
                )],
                FinishReason::ToolCalls,
            ),
        )),
        unmetered(text_reply(
            "Turn in progress\nIn between: Read a.rs.\nT1: read a.rs",
        )),
        unmetered(file_calls(&[(
            "call-2",
            "write_file",
            r#"{"path":"a.rs"}"#,
        )])),
        unmetered(text_reply("rewrote it")),
        unmetered(text_reply("next answer")),
    ]);
    let (agent, _) = windowed_with_tools(
        &provider,
        45_000,
        64,
        vec![file_tool("read_file"), file_tool("write_file")],
    );
    let (log, entries) = MemoryLog::shared();
    let mut agent = logged(agent, log);
    let (report, _) = run(&mut agent, "rewrite a.rs").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let entries = entries.lock().unwrap().clone();
    assert!(
        matches!(&entries[0], Logged::Compaction { steps, .. } if steps.len() == 1),
        "{entries:?}"
    );
    let Some(Logged::Turn { files, .. }) = entries.last() else {
        panic!("{entries:?}");
    };
    assert_eq!(
        files,
        &["read a.rs full stale".to_owned(), "write a.rs".to_owned()]
    );
    run(&mut agent, "and now?").await;
    let requests = provider.requests();
    let messages = &requests.last().unwrap().messages;
    let evidence = messages
        .iter()
        .position(
            |message| matches!(message, ChatMessage::User { content, .. } if content == EVIDENCE),
        )
        .expect("the evidence message");
    assert!(
        matches!(&messages[evidence - 1], ChatMessage::Tool { call_id, .. } if call_id.as_str() == "call-2")
    );
}

fn whole_read(path: &str, call_id: &str) -> FileEvidence {
    FileEvidence {
        path: path.to_owned(),
        new_path: None,
        tool_call_id: call_id.to_owned(),
        tool_name: "read_file".to_owned(),
        action: FileEvidenceAction::Read,
        status: ToolResultStatus::Success,
        model_view_covers_full_file: true,
        stale: false,
    }
}

#[tokio::test]
async fn a_continued_turn_keeps_its_checkpoints_evidence_and_adds_its_own() {
    for write_id in ["call-2", "call-1"] {
        let provider = FakeProvider::new(vec![
            file_calls(&[(write_id, "write_file", r#"{"path":"a.rs"}"#)]),
            text_reply("rewrote it"),
        ]);
        let (log, entries) = MemoryLog::shared();
        let mut agent = logged(file_agent(&provider), log);
        let recovered = RecoveredTurn {
            prompt: "rewrite a.rs".to_owned(),
            messages: vec![
                ChatMessage::Assistant {
                    content: None,
                    tool_calls: vec![ToolCall::new("call-1", "read_file", r#"{"path":"a.rs"}"#)],
                    provider_replay: None,
                },
                ChatMessage::Tool {
                    call_id: ToolCallId::new("call-1"),
                    tool_name: "read_file".to_owned(),
                    content: "text".to_owned(),
                    status: ToolResultStatus::Success,
                },
            ],
            outputs: Vec::new(),
            files: vec![
                whole_read("gone.rs", "call-0"),
                whole_read("a.rs", "call-1"),
            ],
            source: String::new(),
            source_presented: false,
            tool_state: RecoveryToolState::Confirmed,
            strategy: RecoveryStrategy::ContinueAfterTool,
            fast_mode: false,
        };
        let report = agent
            .continue_turn(recovered, &mut |_| {}, &CancellationToken::new())
            .await;
        assert_eq!(report.outcome, TurnOutcome::Completed);
        let entries = entries.lock().unwrap().clone();
        let Some(Logged::Turn { files, .. }) = entries.last() else {
            panic!("{entries:?}");
        };
        assert_eq!(
            files,
            &[
                "read gone.rs full".to_owned(),
                "read a.rs full stale".to_owned(),
                "write a.rs".to_owned(),
            ],
            "{write_id}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_recovery_checkpoint_carries_the_turns_evidence() {
    let provider = FakeProvider::new(vec![
        file_calls(&[("call-1", "read_file", r#"{"path":"a.rs","whole":1}"#)]),
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::ServerError, "server_error"),
        ),
        text_reply("done"),
    ]);
    let (log, entries) = MemoryLog::shared();
    let mut agent = logged(file_agent(&provider), log);
    run(&mut agent, "read a.rs").await;
    let entries = entries.lock().unwrap().clone();
    let Some(Logged::Recovery {
        files, progress, ..
    }) = entries.first()
    else {
        panic!("{entries:?}");
    };
    assert!(matches!(progress, RecoveryProgress::Waiting(_)));
    assert_eq!(files, &["read a.rs full".to_owned()]);
}
