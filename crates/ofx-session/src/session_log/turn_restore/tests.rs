use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use ofx_config::ProviderId;
use ofx_contract::{
    HistoryCut, HistorySteering, HistoryStep, HistoryTurn, ProviderReplay, ReasoningEffort,
    ReplaySource, StepResult, TurnEnd, TurnStop,
};
use serde_json::Value;

use super::*;
use crate::session_codec::{SavedProvider, SessionMetadata, SessionPreferences};
use crate::session_event::{InterruptReason, InterruptedEvent, SteeringEvent, UserEvent};
use crate::session_log::conversation_history::SavedTurn;
use crate::session_log::{LOCK_DEADLINE, WritableSession, resume_session, start_session};

struct Fixture {
    root: tempfile::TempDir,
    sessions: PrivateDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let data = PrivateDir::open_or_create(&root.path().join("data")).unwrap();
        let sessions = data.open_or_create_child("sessions").unwrap();
        Self { root, sessions }
    }

    fn dir(&self) -> PathBuf {
        self.root.path().join("data/sessions/restored")
    }

    fn start(&self) -> WritableSession {
        start_session(&self.sessions, metadata()).unwrap()
    }

    fn resumed(&self) -> RestoredHistory {
        resume_session(&self.sessions, "restored", LOCK_DEADLINE)
            .unwrap()
            .restored_history()
            .unwrap()
    }

    fn frames(&self) -> Vec<Value> {
        fs::read_to_string(self.dir().join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn events(&self) -> Vec<(u64, String)> {
        self.frames()
            .iter()
            .map(|frame| {
                let kind = frame["event"].as_object().unwrap().keys().next().unwrap();
                (frame["seq"].as_u64().unwrap(), kind.clone())
            })
            .collect()
    }
}

fn metadata() -> SessionMetadata {
    SessionMetadata {
        id: "restored".to_owned(),
        origin_workspace_root: "/workspace".to_owned(),
        workspace_root: "/workspace".to_owned(),
        created_at_ms: 1,
        updated_at_ms: 1,
        conversation_language: "und".to_owned(),
        preferences: SessionPreferences {
            provider: gateway(),
            model: "openai/gpt-5".to_owned(),
            effort: ReasoningEffort::Auto,
            fast_mode: false,
        },
        title: None,
    }
}

fn gateway() -> SavedProvider {
    SavedProvider::new(ProviderId::Gateway, None).unwrap()
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(id),
        name: name.to_owned(),
        arguments: format!(r#"{{"id":"{id}"}}"#),
    }
}

fn replay(provider: &str, parts: &str) -> ProviderReplay {
    ProviderReplay {
        source: ReplaySource {
            provider: provider.to_owned(),
            model: "gpt-5.4".to_owned(),
        },
        parts_json: parts.to_owned(),
    }
}

fn result<'a>(call: &'a ToolCall, output: &'a str, status: ToolResultStatus) -> StepResult<'a> {
    StepResult {
        call_id: call.id.as_str(),
        tool_name: &call.name,
        output,
        output_bytes: output.len(),
        status,
    }
}

fn step<'a>(
    assistant: &'a str,
    calls: &'a [ToolCall],
    results: Vec<StepResult<'a>>,
) -> HistoryStep<'a> {
    HistoryStep {
        assistant,
        provider_replay: None,
        tool_calls: calls,
        tool_results: results,
    }
}

fn replied(text: &str) -> TurnEnd<'_> {
    TurnEnd::Replied {
        text,
        provider_replay: None,
    }
}

fn assistant(content: Option<&str>, calls: &[ToolCall]) -> ChatMessage {
    ChatMessage::Assistant {
        content: content.map(str::to_owned),
        tool_calls: calls.to_vec(),
        provider_replay: None,
    }
}

fn tool(call: &ToolCall, content: &str, status: ToolResultStatus) -> ChatMessage {
    ChatMessage::Tool {
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: content.to_owned(),
        status,
    }
}

fn simple_turn<'a>(user: &'a str, reply: &'a str) -> HistoryTurn<'a> {
    HistoryTurn {
        user,
        steps: Vec::new(),
        steering: Vec::new(),
        end: replied(reply),
    }
}

#[test]
fn recorded_turns_restore_the_messages_the_model_saw() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let read = [call("call-1", "read_file")];
    let shell = [call("call-2", "shell")];
    let large = "y".repeat(10 * 1024);
    let codex = replay("codex", r#"[{"type":"reasoning"}]"#);
    let turn = HistoryTurn {
        user: "inspect",
        steps: vec![
            HistoryStep {
                provider_replay: Some(&codex),
                ..step(
                    "Looking",
                    &read,
                    vec![result(&read[0], "contents", ToolResultStatus::Success)],
                )
            },
            step(
                "",
                &shell,
                vec![StepResult {
                    output_bytes: 70_000,
                    ..result(&shell[0], &large, ToolResultStatus::Failure)
                }],
            ),
        ],
        steering: Vec::new(),
        end: TurnEnd::Replied {
            text: "done",
            provider_replay: Some(&codex),
        },
    };
    session.record_turn(&turn, &gateway()).unwrap();
    drop(session);

    let frames = fixture.frames();
    assert_eq!(
        fixture.events(),
        [
            (1, "user".to_owned()),
            (2, "assistant".to_owned()),
            (3, "tool_call".to_owned()),
            (4, "tool_result".to_owned()),
            (5, "tool_call".to_owned()),
            (6, "tool_result".to_owned()),
            (7, "assistant".to_owned()),
            (8, "turn_completed".to_owned()),
        ]
    );
    assert_eq!(
        frames[1]["event"]["assistant"]["provider_replay"]["source"]["provider"],
        "codex"
    );
    assert_eq!(
        frames[1]["event"]["assistant"]["standalone_response"],
        false
    );
    let large_result = &frames[5]["event"]["tool_result"];
    let handle = large_result["artifact_ref"].as_str().unwrap();
    assert!(handle.starts_with("result-shell-"), "{handle}");
    assert_eq!(large_result["stored_bytes"], 10 * 1024);
    assert_eq!(large_result["output_bytes"], 70_000);
    assert_eq!(frames[3]["event"]["tool_result"]["output_bytes"], 8);
    assert_eq!(large_result["completeness"], "complete");
    assert_eq!(large_result["status"], "failure");
    assert_eq!(
        large_result["preview"].as_str().unwrap().len(),
        crate::result_store::PREVIEW_BYTES
    );
    let artifact = fixture.dir().join("tool-results").join(handle);
    assert_eq!(fs::read_to_string(&artifact).unwrap(), large);
    assert_eq!(
        fs::metadata(&artifact).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let restored = fixture.resumed();
    assert_eq!(restored.checkpoint, None);
    assert_eq!(restored.turn_starts, [0]);
    assert_eq!(
        restored.messages,
        [
            ChatMessage::user("inspect"),
            ChatMessage::Assistant {
                content: Some("Looking".to_owned()),
                tool_calls: read.to_vec(),
                provider_replay: Some(codex.clone()),
            },
            tool(&read[0], "contents", ToolResultStatus::Success),
            assistant(None, &shell),
            tool(&shell[0], &large, ToolResultStatus::Failure),
            ChatMessage::Assistant {
                content: Some("done".to_owned()),
                tool_calls: Vec::new(),
                provider_replay: Some(codex),
            },
        ]
    );
}

#[test]
fn standalone_steps_and_empty_replies_follow_upstream_boundaries() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let reasoning = replay("codex", "[]");
    let turn = HistoryTurn {
        user: "think",
        steps: vec![HistoryStep {
            provider_replay: Some(&reasoning),
            ..step("", &[], Vec::new())
        }],
        steering: Vec::new(),
        end: replied(""),
    };
    session.record_turn(&turn, &gateway()).unwrap();
    drop(session);
    let frames = fixture.frames();
    assert_eq!(frames[1]["event"]["assistant"]["standalone_response"], true);
    assert_eq!(frames[2]["event"]["assistant"]["text"], "");
    assert_eq!(
        frames[2]["event"]["assistant"]["standalone_response"],
        false
    );
    assert_eq!(
        fixture.resumed().messages,
        [
            ChatMessage::user("think"),
            ChatMessage::Assistant {
                content: Some(String::new()),
                tool_calls: Vec::new(),
                provider_replay: Some(reasoning),
            },
        ]
    );
}

#[test]
fn interrupted_turns_restore_with_upstream_closing_messages() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let read = [call("call-1", "read_file")];
    session
        .record_turn(
            &HistoryTurn {
                user: "stop",
                steps: Vec::new(),
                steering: Vec::new(),
                end: TurnEnd::Stopped {
                    reason: TurnStop::Cancelled,
                    partial: "half",
                },
            },
            &gateway(),
        )
        .unwrap();
    session
        .record_turn(
            &HistoryTurn {
                user: "nothing",
                steps: Vec::new(),
                steering: Vec::new(),
                end: TurnEnd::Stopped {
                    reason: TurnStop::Failed,
                    partial: "",
                },
            },
            &gateway(),
        )
        .unwrap();
    session
        .record_turn(
            &HistoryTurn {
                user: "worked",
                steps: vec![step(
                    "",
                    &read,
                    vec![result(&read[0], "ok", ToolResultStatus::Success)],
                )],
                steering: Vec::new(),
                end: TurnEnd::Stopped {
                    reason: TurnStop::Failed,
                    partial: "",
                },
            },
            &gateway(),
        )
        .unwrap();
    drop(session);
    let frames = fixture.frames();
    assert_eq!(frames.len(), 2 + 4);
    assert_eq!(frames[1]["event"]["interrupted"]["reason"], "cancelled");
    assert_eq!(frames[1]["event"]["interrupted"]["partial_text"], "half");
    assert_eq!(
        frames[5]["event"]["interrupted"]["partial_text"],
        Value::Null
    );
    let closed = |text: &str| assistant(Some(text), &[]);
    let restored = fixture.resumed();
    assert_eq!(restored.turn_starts, [0, 3]);
    assert_eq!(
        restored.messages,
        [
            ChatMessage::user("stop"),
            closed("half\n\nThe previous response ended before completion."),
            ChatMessage::user(INTERRUPTED_TURN_CONTEXT),
            ChatMessage::user("worked"),
            assistant(None, &read),
            tool(&read[0], "ok", ToolResultStatus::Success),
            closed("The previous response ended before completion."),
            ChatMessage::user(INTERRUPTED_TURN_CONTEXT),
        ]
    );
}

#[test]
fn a_mid_turn_checkpoint_covers_the_cut_and_the_rest_of_the_turn_follows_it() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session
        .record_turn(&simple_turn("first", "one"), &gateway())
        .unwrap();
    let first = [call("call-1", "read_file")];
    let second = [call("call-2", "read_file")];
    let active = HistoryTurn {
        user: "second",
        steps: vec![
            step(
                "",
                &first,
                vec![result(&first[0], "a", ToolResultStatus::Success)],
            ),
            step(
                "",
                &second,
                vec![result(&second[0], "b", ToolResultStatus::Success)],
            ),
        ],
        steering: Vec::new(),
        end: replied(""),
    };
    let cut = HistoryCut {
        turns: 1,
        tool_steps: 1,
        ..HistoryCut::default()
    };
    session
        .record_compaction("SUMMARY", cut, Some(&active), &gateway())
        .unwrap();
    assert!(session.turn_open());
    let frames = fixture.frames();
    assert_eq!(frames.len(), 3 + 6);
    assert_eq!(
        frames[8]["event"]["context_checkpoint"],
        serde_json::json!({"covers_through_seq": 6, "summary": "SUMMARY"})
    );
    let rest = HistoryTurn {
        user: "second",
        steps: vec![step(
            "",
            &second,
            vec![result(&second[0], "b", ToolResultStatus::Success)],
        )],
        steering: Vec::new(),
        end: replied("done"),
    };
    session.record_turn(&rest, &gateway()).unwrap();
    assert!(!session.turn_open());
    drop(session);
    assert_eq!(
        fixture.events()[9..],
        [
            (10, "assistant".to_owned()),
            (11, "turn_completed".to_owned())
        ]
    );
    let restored = fixture.resumed();
    assert_eq!(restored.checkpoint.as_deref(), Some("SUMMARY"));
    assert_eq!(restored.turn_starts, [0]);
    assert_eq!(
        restored.messages,
        [
            ChatMessage::user("second"),
            assistant(None, &second),
            tool(&second[0], "b", ToolResultStatus::Success),
            assistant(Some("done"), &[]),
        ]
    );
}

#[test]
fn a_checkpoint_at_a_turn_boundary_keeps_the_running_turn_after_it() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session
        .record_turn(&simple_turn("first", "one"), &gateway())
        .unwrap();
    let cut = HistoryCut {
        turns: 1,
        tool_steps: 0,
        ..HistoryCut::default()
    };
    session
        .record_compaction("S", cut, Some(&simple_turn("second", "")), &gateway())
        .unwrap();
    session
        .record_compaction("S2", cut, Some(&simple_turn("second", "")), &gateway())
        .unwrap_err();
    let cut = HistoryCut {
        turns: 0,
        tool_steps: 0,
        ..HistoryCut::default()
    };
    session
        .record_compaction("S2", cut, Some(&simple_turn("second", "")), &gateway())
        .unwrap();
    session
        .record_turn(&simple_turn("second", "two"), &gateway())
        .unwrap();
    drop(session);
    let frames = fixture.frames();
    assert_eq!(
        frames[4]["event"]["context_checkpoint"]["covers_through_seq"],
        3
    );
    assert_eq!(
        frames[5]["event"]["context_checkpoint"]["covers_through_seq"],
        3
    );
    let restored = fixture.resumed();
    assert_eq!(restored.checkpoint.as_deref(), Some("S2"));
    assert_eq!(
        restored.messages,
        [ChatMessage::user("second"), assistant(Some("two"), &[])]
    );
}

#[test]
fn a_checkpoint_between_turns_is_written_alone_and_keeps_the_turns_after_its_cut() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    for (prompt, reply) in [("first", "one"), ("second", "two"), ("third", "three")] {
        session
            .record_turn(&simple_turn(prompt, reply), &gateway())
            .unwrap();
    }
    let cut = HistoryCut {
        turns: 2,
        tool_steps: 0,
        ..HistoryCut::default()
    };
    session
        .record_compaction("S", cut, None, &gateway())
        .unwrap();
    assert!(!session.turn_open());
    session
        .record_turn(&simple_turn("fourth", "four"), &gateway())
        .unwrap();
    drop(session);
    assert_eq!(fixture.events()[9], (10, "context_checkpoint".to_owned()));
    assert_eq!(
        fixture.frames()[9]["event"]["context_checkpoint"]["covers_through_seq"],
        6
    );
    let restored = fixture.resumed();
    assert_eq!(restored.checkpoint.as_deref(), Some("S"));
    assert_eq!(
        restored.messages,
        [
            ChatMessage::user("third"),
            assistant(Some("three"), &[]),
            ChatMessage::user("fourth"),
            assistant(Some("four"), &[]),
        ]
    );
}

#[test]
fn a_crash_after_a_mid_turn_checkpoint_closes_the_turn_on_resume() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let first = [call("call-1", "read_file")];
    let second = [call("call-2", "read_file")];
    let active = HistoryTurn {
        user: "work",
        steps: vec![
            step(
                "",
                &first,
                vec![result(&first[0], "a", ToolResultStatus::Success)],
            ),
            step(
                "",
                &second,
                vec![result(&second[0], "b", ToolResultStatus::Success)],
            ),
        ],
        steering: Vec::new(),
        end: replied(""),
    };
    let cut = HistoryCut {
        turns: 0,
        tool_steps: 1,
        ..HistoryCut::default()
    };
    session
        .record_compaction("S", cut, Some(&active), &gateway())
        .unwrap();
    drop(session);
    let restored = fixture.resumed();
    assert_eq!(restored.checkpoint.as_deref(), Some("S"));
    assert_eq!(
        restored.messages,
        [
            ChatMessage::user("work"),
            assistant(None, &second),
            tool(&second[0], "b", ToolResultStatus::Success),
            assistant(Some(INTERRUPTED_BEFORE_COMPLETION), &[]),
            ChatMessage::user(INTERRUPTED_TURN_CONTEXT),
        ]
    );
}

#[test]
fn checkpoint_prefixes_must_not_carry_a_reply() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    assert_eq!(
        session.record_compaction(
            "S",
            HistoryCut::default(),
            Some(&simple_turn("q", "answer")),
            &gateway()
        ),
        Err(SessionError::InvalidConversationEvent)
    );
    let missing = HistoryCut {
        turns: 3,
        tool_steps: 0,
        ..HistoryCut::default()
    };
    assert_eq!(
        session.record_compaction("S", missing, Some(&simple_turn("q", "")), &gateway()),
        Err(SessionError::InvalidContextHistoryStart)
    );
    assert_eq!(fixture.frames().len(), 0);
}

#[test]
fn restored_results_fall_back_when_their_artifact_is_missing_or_changed() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let calls = [call("call-1", "shell"), call("call-2", "shell")];
    let large = "z".repeat(5000);
    let turn = HistoryTurn {
        user: "run",
        steps: vec![step(
            "",
            &calls,
            vec![
                result(&calls[0], &large, ToolResultStatus::Success),
                result(&calls[1], "small", ToolResultStatus::Success),
            ],
        )],
        steering: Vec::new(),
        end: replied("ok"),
    };
    session.record_turn(&turn, &gateway()).unwrap();
    drop(session);
    let results = fixture.dir().join("tool-results");
    for entry in fs::read_dir(&results).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    let messages = fixture.resumed().messages;
    assert_eq!(
        messages[2],
        tool(&calls[0], RESULT_UNAVAILABLE, ToolResultStatus::Success)
    );
    assert_eq!(
        messages[3],
        tool(&calls[1], "small", ToolResultStatus::Success)
    );
}

#[test]
fn replays_are_saved_only_with_a_provider_identity_that_reads_back() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let router =
        SavedProvider::new(ProviderId::Configured("router".to_owned()), Some([7; 32])).unwrap();
    let own = replay("router", "own");
    let other = replay("other", "other");
    let codex = replay("codex", "codex");
    for (provider, replay) in [(&router, &own), (&router, &other), (&gateway(), &codex)] {
        let turn = HistoryTurn {
            user: "q",
            steps: Vec::new(),
            steering: Vec::new(),
            end: TurnEnd::Replied {
                text: "a",
                provider_replay: Some(replay),
            },
        };
        session.record_turn(&turn, provider).unwrap();
    }
    drop(session);
    let frames = fixture.frames();
    let saved = |index: usize| frames[index]["event"]["assistant"]["provider_replay"].clone();
    assert_eq!(saved(1)["source"]["provider"]["name"], "router");
    assert_eq!(saved(1)["source"]["provider"]["binding"], "07".repeat(32));
    assert_eq!(saved(4), Value::Null);
    assert_eq!(saved(7)["source"]["provider"], "codex");
    let restored = fixture.resumed();
    assert_eq!(
        restored.messages[1],
        ChatMessage::Assistant {
            content: Some("a".to_owned()),
            tool_calls: Vec::new(),
            provider_replay: Some(own),
        }
    );
}

fn saved_turn(events: Vec<ConversationEvent>) -> SavedTurn {
    SavedTurn { events }
}

fn user(text: &str) -> ConversationEvent {
    ConversationEvent::User(UserEvent::new(text))
}

fn saved_call(id: &str) -> ConversationEvent {
    ConversationEvent::ToolCall(ToolCallEvent::new(
        id,
        "shell",
        "{}",
        ofx_contract::ToolArgumentIntegrity::Valid,
    ))
}

fn saved_result(id: &str, completeness: ArtifactCompleteness) -> ConversationEvent {
    let mut event = ToolResultEvent::new(
        id,
        "shell",
        ToolResultStatus::Success,
        "result-shell-1-2.txt",
        9000,
        completeness,
    );
    event.preview = Some("head".to_owned());
    ConversationEvent::ToolResult(event)
}

fn saved_assistant(text: &str) -> ConversationEvent {
    ConversationEvent::Assistant(AssistantEvent {
        text: text.to_owned(),
        provider_replay: None,
        standalone_response: false,
    })
}

fn stopped(partial: Option<&str>) -> ConversationEvent {
    ConversationEvent::Interrupted(InterruptedEvent::new(
        InterruptReason::Cancelled,
        partial.map(str::to_owned),
    ))
}

fn completed() -> ConversationEvent {
    ConversationEvent::TurnCompleted(crate::session_event::TurnCompletedEvent::default())
}

fn restore(turns: Vec<SavedTurn>) -> Result<RestoredHistory, SessionError> {
    let fixture = Fixture::new();
    let session = fixture.start();
    restored_history(
        SavedHistory {
            compacted: None,
            turns,
        },
        &session.owned.dir,
    )
}

#[test]
fn saved_turns_written_by_upstream_restore_as_upstream_projects_them() {
    let restored = restore(vec![
        saved_turn(vec![
            user("partial"),
            saved_call("c1"),
            saved_result("c1", ArtifactCompleteness::Partial),
            completed(),
        ]),
        saved_turn(vec![
            user("steer"),
            saved_assistant("prefix"),
            ConversationEvent::Steering(SteeringEvent {
                text: "also this".to_owned(),
            }),
            saved_assistant("final"),
            completed(),
        ]),
        saved_turn(vec![user("aborted"), saved_call("c2"), stopped(Some("p"))]),
    ])
    .unwrap();
    let shell = |id: &str| ToolCall {
        id: ToolCallId::new(id),
        name: "shell".to_owned(),
        arguments: "{}".to_owned(),
    };
    assert_eq!(restored.turn_starts, [0, 3, 7]);
    assert_eq!(
        restored.messages,
        [
            ChatMessage::user("partial"),
            assistant(None, &[shell("c1")]),
            tool(
                &shell("c1"),
                &format_stored_result_output("result-shell-1-2.txt", "head", 9000),
                ToolResultStatus::Success
            ),
            ChatMessage::user("steer"),
            assistant(Some("prefix"), &[]),
            ChatMessage::restored_steering("also this"),
            assistant(Some("final"), &[]),
            ChatMessage::user("aborted"),
            assistant(Some("p"), &[shell("c2")]),
            tool(&shell("c2"), ABORTED_TOOL_OUTPUT, ToolResultStatus::Failure),
            ChatMessage::user(INTERRUPTED_TURN_CONTEXT),
        ]
    );
}

#[test]
fn malformed_saved_turns_are_refused() {
    for events in [
        vec![
            user("q"),
            saved_result("c1", ArtifactCompleteness::Complete),
            completed(),
        ],
        vec![user("q"), saved_call("c1"), completed()],
        vec![user("q"), saved_call("c1"), saved_call("c2"), stopped(None)],
        vec![saved_assistant("no user"), completed()],
        vec![user("q"), completed(), completed()],
        vec![user("q")],
    ] {
        assert_eq!(
            restore(vec![saved_turn(events.clone())]),
            Err(SessionError::InvalidConversationFrame),
            "{events:?}"
        );
    }
}

#[test]
fn a_turn_left_open_by_a_failed_save_blocks_every_later_save() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session
        .record_turn(&simple_turn("first", "one"), &gateway())
        .unwrap();
    let cut = HistoryCut {
        turns: 1,
        tool_steps: 0,
        ..HistoryCut::default()
    };
    session
        .record_compaction("S", cut, Some(&simple_turn("second", "")), &gateway())
        .unwrap();
    assert_eq!(session.require_writable(), Ok(()));
    session.writer.fail_next_syncs(1);
    assert_eq!(
        session.record_turn(&simple_turn("second", "two"), &gateway()),
        Err(SessionError::Io(std::io::ErrorKind::Other))
    );
    assert!(session.turn_open());
    assert_eq!(
        session.require_writable(),
        Err(SessionError::SessionCommitFailed)
    );
    assert_eq!(
        session.record_turn(&simple_turn("third", "three"), &gateway()),
        Err(SessionError::SessionCommitFailed)
    );
    assert_eq!(
        session.record_compaction("S2", HistoryCut::default(), None, &gateway()),
        Err(SessionError::SessionCommitFailed)
    );
}

#[test]
fn a_turn_whose_results_cannot_be_stored_after_a_checkpoint_blocks_every_later_save() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session
        .record_turn(&simple_turn("first", "one"), &gateway())
        .unwrap();
    let cut = HistoryCut {
        turns: 1,
        tool_steps: 0,
        ..HistoryCut::default()
    };
    session
        .record_compaction("S", cut, Some(&simple_turn("second", "")), &gateway())
        .unwrap();
    let results = fixture.dir().join("tool-results");
    fs::write(&results, "not a directory").unwrap();
    let calls = [call("c1", "read_file")];
    let second = HistoryTurn {
        user: "second",
        steps: vec![step(
            "",
            &calls,
            vec![result(&calls[0], "contents", ToolResultStatus::Success)],
        )],
        steering: Vec::new(),
        end: replied("two"),
    };
    assert!(session.record_turn(&second, &gateway()).is_err());
    assert!(session.turn_open());
    assert_eq!(
        session.require_writable(),
        Err(SessionError::SessionCommitFailed)
    );
    fs::remove_file(&results).unwrap();
    assert_eq!(
        session.record_turn(&simple_turn("third", "three"), &gateway()),
        Err(SessionError::SessionCommitFailed)
    );
    assert_eq!(
        fixture.events(),
        [
            (1, "user".to_owned()),
            (2, "assistant".to_owned()),
            (3, "turn_completed".to_owned()),
            (4, "user".to_owned()),
            (5, "context_checkpoint".to_owned()),
        ]
    );
}

#[test]
fn a_failed_save_of_a_closed_log_leaves_later_saves_allowed() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.writer.fail_next_syncs(1);
    assert!(
        session
            .record_turn(&simple_turn("first", "one"), &gateway())
            .is_err()
    );
    assert_eq!(session.require_writable(), Ok(()));
    assert_eq!(session.last_seq(), 0);
    session
        .record_turn(&simple_turn("second", "two"), &gateway())
        .unwrap();
    assert_eq!(session.last_seq(), 3);
}

#[test]
fn saved_tool_results_read_back_whole_from_their_preview_or_their_artifact() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let calls = [
        call("call-1", "ask_user_question"),
        call("call-2", "ask_user_question"),
    ];
    let large = "z".repeat(10 * 1024);
    let turn = HistoryTurn {
        user: "ask",
        steps: vec![step(
            "",
            &calls,
            vec![
                result(&calls[0], "short", ToolResultStatus::Success),
                result(&calls[1], &large, ToolResultStatus::Success),
            ],
        )],
        steering: Vec::new(),
        end: replied("done"),
    };
    session.record_turn(&turn, &gateway()).unwrap();
    drop(session);
    let outputs = |session: &WritableSession| {
        let mut outputs = Vec::new();
        session
            .visit_transcript(|turn| {
                for event in turn.events {
                    if let ConversationEvent::ToolResult(result) = event {
                        outputs.push(session.tool_result_output(&result));
                    }
                }
            })
            .unwrap();
        outputs
    };
    let resumed = resume_session(&fixture.sessions, "restored", LOCK_DEADLINE).unwrap();
    assert_eq!(
        outputs(&resumed),
        [Some("short".to_owned()), Some(large.clone())]
    );
    for entry in fs::read_dir(fixture.dir().join("tool-results")).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    assert_eq!(outputs(&resumed), [Some("short".to_owned()), None]);
}

#[test]
fn saved_results_larger_than_replay_limit_keep_the_complete_sidecar() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let calls = [call("large-result", "read_file")];
    let output = "z".repeat(8 * 1024 * 1024 + 1);
    let turn = HistoryTurn {
        user: "read",
        steps: vec![step(
            "",
            &calls,
            vec![result(&calls[0], &output, ToolResultStatus::Success)],
        )],
        steering: Vec::new(),
        end: replied("done"),
    };
    session.record_turn(&turn, &gateway()).unwrap();
    drop(session);
    let resumed = resume_session(&fixture.sessions, "restored", LOCK_DEADLINE).unwrap();
    let mut found = false;
    resumed
        .visit_transcript(|turn| {
            for event in turn.events {
                if let ConversationEvent::ToolResult(result) = event {
                    found = true;
                    assert_eq!(result.stored_bytes, u64::try_from(output.len()).unwrap());
                    assert_eq!(resumed.tool_result_output(&result), None);
                    assert_eq!(
                        fs::read(
                            fixture
                                .dir()
                                .join("tool-results")
                                .join(&result.artifact_ref)
                        )
                        .unwrap(),
                        output.as_bytes()
                    );
                }
            }
        })
        .unwrap();
    assert!(found);
}

fn saved_title(fixture: &Fixture) -> Value {
    let manifest = fs::read_to_string(fixture.dir().join("session.json")).unwrap();
    serde_json::from_str::<Value>(&manifest).unwrap()["title"].clone()
}

#[test]
fn only_the_first_save_of_a_fresh_session_names_it() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session
        .record_turn(&simple_turn("  fix the\tbuild\nplease", "ok"), &gateway())
        .unwrap();
    assert_eq!(saved_title(&fixture), "fix the build");
    session
        .record_turn(&simple_turn("rename me", "no"), &gateway())
        .unwrap();
    assert_eq!(saved_title(&fixture), "fix the build");
    assert_eq!(session.display_title(), "fix the build");

    let fixture = Fixture::new();
    let mut session = fixture.start();
    session
        .record_turn(&simple_turn("/help", "commands"), &gateway())
        .unwrap();
    session
        .record_turn(&simple_turn("named too late", "ok"), &gateway())
        .unwrap();
    assert_eq!(saved_title(&fixture), Value::Null);
    drop(session);
    let resumed = resume_session(&fixture.sessions, "restored", LOCK_DEADLINE).unwrap();
    assert_eq!(resumed.display_title(), "named too late");

    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.select_model("openai/gpt-5-mini", false).unwrap();
    session
        .record_turn(&simple_turn("after a model choice", "ok"), &gateway())
        .unwrap();
    assert_eq!(saved_title(&fixture), Value::Null);
}

#[test]
fn a_title_that_cannot_be_written_leaves_the_turn_saved() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let manifest = fixture.dir().join("session.json");
    fs::remove_file(&manifest).unwrap();
    fs::create_dir(&manifest).unwrap();
    session
        .record_turn(&simple_turn("named", "ok"), &gateway())
        .unwrap();
    assert_eq!(session.require_writable(), Ok(()));
    assert_eq!(session.last_seq(), 3);
}

#[test]
fn a_title_saved_before_the_first_turn_is_kept() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let mut renamed = session.metadata.clone();
    renamed.title = Some("Renamed by hand".to_owned());
    session.write_metadata(renamed).unwrap();
    session
        .record_turn(&simple_turn("fix the build", "ok"), &gateway())
        .unwrap();
    assert_eq!(saved_title(&fixture), "Renamed by hand");
    assert_eq!(session.display_title(), "Renamed by hand");
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let mut renamed = session.metadata.clone();
    renamed.title = Some("Generated title".to_owned());
    session.write_metadata(renamed).unwrap();
    session
        .record_compaction(
            "S",
            HistoryCut {
                turns: 0,
                tool_steps: 0,
                steering: 0,
            },
            Some(&simple_turn("compacted first", "")),
            &gateway(),
        )
        .unwrap();
    assert_eq!(saved_title(&fixture), "Generated title");
}

#[test]
fn a_resumed_session_or_a_checkpointed_first_turn_keeps_titles_as_upstream() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let cut = HistoryCut {
        turns: 0,
        tool_steps: 0,
        steering: 0,
    };
    session
        .record_compaction(
            "S",
            cut,
            Some(&simple_turn("compacted first", "")),
            &gateway(),
        )
        .unwrap();
    assert_eq!(saved_title(&fixture), "compacted first");
    drop(session);
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session
        .record_compaction("S", cut, None, &gateway())
        .unwrap();
    assert_eq!(saved_title(&fixture), Value::Null);
    drop(session);
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session
        .record_turn(&simple_turn("/model", "listed"), &gateway())
        .unwrap();
    drop(session);
    let mut resumed = resume_session(&fixture.sessions, "restored", LOCK_DEADLINE).unwrap();
    resumed
        .record_turn(&simple_turn("named later", "ok"), &gateway())
        .unwrap();
    assert_eq!(saved_title(&fixture), Value::Null);
}

fn steering<'a>(text: &'a str, assistant_prefix: &'a str, after: usize) -> HistorySteering<'a> {
    HistorySteering {
        text,
        assistant_prefix,
        after_tool_step_count: after,
    }
}

#[test]
fn steering_is_saved_at_its_step_boundary_and_restored_as_plain_user_text() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let read = [call("call-1", "read_file")];
    let turn = HistoryTurn {
        user: "go",
        steps: vec![step(
            "",
            &read,
            vec![result(&read[0], "a", ToolResultStatus::Success)],
        )],
        steering: vec![steering("first", "Looking", 0), steering("second", "", 1)],
        end: replied("done"),
    };
    session.record_turn(&turn, &gateway()).unwrap();
    drop(session);
    let kinds: Vec<String> = fixture.events().into_iter().map(|(_, kind)| kind).collect();
    assert_eq!(
        kinds,
        [
            "user",
            "assistant",
            "steering",
            "tool_call",
            "tool_result",
            "steering",
            "assistant",
            "turn_completed"
        ]
    );
    let frames = fixture.frames();
    assert_eq!(
        frames[2]["event"],
        serde_json::json!({"steering": {"text": "first"}})
    );
    assert_eq!(
        frames[1]["event"]["assistant"]["standalone_response"],
        false
    );
    assert_eq!(
        fixture.resumed().messages,
        [
            ChatMessage::user("go"),
            assistant(Some("Looking"), &[]),
            ChatMessage::restored_steering("first"),
            assistant(None, &read),
            tool(&read[0], "a", ToolResultStatus::Success),
            ChatMessage::restored_steering("second"),
            assistant(Some("done"), &[]),
        ]
    );
}

#[test]
fn a_turn_that_only_took_steering_is_saved() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let turn = HistoryTurn {
        user: "go",
        steps: Vec::new(),
        steering: vec![steering("change course", "partial", 0)],
        end: TurnEnd::Stopped {
            reason: TurnStop::Failed,
            partial: "",
        },
    };
    session.record_turn(&turn, &gateway()).unwrap();
    drop(session);
    assert_eq!(
        fixture.resumed().messages[..3],
        [
            ChatMessage::user("go"),
            assistant(Some("partial"), &[]),
            ChatMessage::restored_steering("change course"),
        ]
    );
}

#[test]
fn steering_past_the_last_step_is_refused() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let turn = HistoryTurn {
        steering: vec![steering("late", "", 1)],
        ..simple_turn("go", "done")
    };
    assert_eq!(
        session.record_turn(&turn, &gateway()),
        Err(SessionError::InvalidConversationEvent)
    );
}

#[test]
fn a_mid_turn_checkpoint_counts_the_steering_it_covers() {
    let first = [call("call-1", "read_file")];
    let second = [call("call-2", "read_file")];
    let active = HistoryTurn {
        user: "work",
        steps: vec![
            step(
                "",
                &first,
                vec![result(&first[0], "a", ToolResultStatus::Success)],
            ),
            step(
                "",
                &second,
                vec![result(&second[0], "b", ToolResultStatus::Success)],
            ),
        ],
        steering: vec![steering("between", "", 1), steering("after", "", 2)],
        end: replied(""),
    };
    for (cut, kept) in [
        (
            HistoryCut {
                turns: 0,
                tool_steps: 1,
                steering: 0,
            },
            vec![
                ChatMessage::user("work"),
                ChatMessage::restored_steering("between"),
                assistant(None, &second),
                tool(&second[0], "b", ToolResultStatus::Success),
                ChatMessage::restored_steering("after"),
            ],
        ),
        (
            HistoryCut {
                turns: 0,
                tool_steps: 2,
                steering: 2,
            },
            vec![ChatMessage::user("work")],
        ),
    ] {
        let fixture = Fixture::new();
        let mut session = fixture.start();
        session
            .record_compaction("S", cut, Some(&active), &gateway())
            .unwrap();
        let rest = HistoryTurn {
            user: "work",
            steps: Vec::new(),
            steering: Vec::new(),
            end: replied("done"),
        };
        let retained_steps = active.steps[cut.tool_steps..].to_vec();
        let retained_steering = active.steering[cut.steering..]
            .iter()
            .map(|entry| HistorySteering {
                after_tool_step_count: entry.after_tool_step_count - cut.tool_steps,
                ..*entry
            })
            .collect();
        let rest = HistoryTurn {
            steps: retained_steps,
            steering: retained_steering,
            ..rest
        };
        session.record_turn(&rest, &gateway()).unwrap();
        drop(session);
        let restored = fixture.resumed();
        let mut expected = kept;
        expected.push(assistant(Some("done"), &[]));
        assert_eq!(restored.messages, expected, "{cut:?}");
    }
}
