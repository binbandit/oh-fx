use std::fs;

use ofx_contract::{ToolArgumentIntegrity, ToolResultStatus};

use super::*;
use crate::session_event::{
    ArtifactCompleteness, AssistantEvent, ContextCheckpointEvent, ToolCallEvent, ToolResultEvent,
    TurnCompletedEvent, UserEvent,
};
use crate::session_log::{ArchivedResult, CompactedHistory, ExecutedStep, TurnExecution};

struct Log {
    root: tempfile::TempDir,
    dir: PrivateDir,
}

impl Log {
    fn new(bytes: &[u8]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let dir = PrivateDir::open_or_create(&root.path().join("session")).unwrap();
        dir.replace(EVENTS_FILE, bytes).unwrap();
        Self { root, dir }
    }

    fn bytes(&self) -> Vec<u8> {
        fs::read(self.root.path().join("session").join(EVENTS_FILE)).unwrap()
    }

    fn boundary(&self) -> Result<RecoveryBoundary, SessionError> {
        classify_conversation_recovery(&self.dir, "session").map(|recovery| recovery.boundary)
    }
}

fn frames(first_seq: u64, events: &[ConversationEvent]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (seq, event) in (first_seq..).zip(events) {
        bytes.extend(encode_conversation_frame(seq, 1, event).unwrap());
    }
    bytes
}

fn user(text: &str) -> ConversationEvent {
    ConversationEvent::User(UserEvent::new(text))
}

fn assistant(text: &str) -> ConversationEvent {
    ConversationEvent::Assistant(AssistantEvent {
        text: text.to_owned(),
        provider_replay: None,
        standalone_response: false,
    })
}

fn completed() -> ConversationEvent {
    ConversationEvent::TurnCompleted(TurnCompletedEvent::default())
}

fn call(id: &str) -> ConversationEvent {
    ConversationEvent::ToolCall(ToolCallEvent::new(
        id,
        "shell",
        "{}",
        ToolArgumentIntegrity::Valid,
    ))
}

fn result(id: &str) -> ConversationEvent {
    ConversationEvent::ToolResult(ToolResultEvent::new(
        id,
        "shell",
        ToolResultStatus::Success,
        format!("result-{id}.txt"),
        0,
        ArtifactCompleteness::Complete,
    ))
}

fn checkpoint(covers_through_seq: u64) -> ConversationEvent {
    ConversationEvent::ContextCheckpoint(ContextCheckpointEvent {
        covers_through_seq,
        summary: "checkpoint".to_owned(),
    })
}

fn interrupted() -> ConversationEvent {
    ConversationEvent::Interrupted(InterruptedEvent::new(InterruptReason::Failed, None))
}

#[test]
fn conversation_recovery_boundary_validates_prefix_and_never_edits_source() {
    let prefix = frames(1, &[user("fact"), assistant("saved"), completed()]);
    let mut batch = frames(
        4,
        &[
            user("unfinished batch"),
            call("one"),
            call("two"),
            interrupted(),
        ],
    );
    batch.extend(b"invalid\n");
    let suffixes: [Vec<u8>; 5] = [
        b"{".to_vec(),
        b"invalid\n".to_vec(),
        frames(99, &[user("wrong sequence")]),
        frames(4, &[checkpoint(9)]),
        batch,
    ];
    for suffix in suffixes {
        let bytes = [prefix.clone(), suffix].concat();
        let log = Log::new(&bytes);
        let boundary = log.boundary().unwrap();
        assert_eq!(boundary.bytes, u64::try_from(prefix.len()).unwrap());
        assert_eq!(boundary.seq, 3);
        assert!(!boundary.turn_open);
        assert_eq!(log.bytes(), bytes);
    }
    assert_eq!(
        Log::new(&prefix).boundary(),
        Err(SessionError::SessionRecoveryNotNeeded)
    );
    assert_eq!(
        Log::new(b"invalid\n").boundary(),
        Err(SessionError::SessionRecoveryBoundaryInvalid)
    );
}

#[test]
fn conversation_recovery_rejects_checkpoint_cuts_inside_tool_batches() {
    let prefix = frames(
        1,
        &[
            user("request"),
            call("one"),
            call("two"),
            result("one"),
            result("two"),
            completed(),
        ],
    );
    for coverage in 1..=6 {
        for tail in [&b""[..], b"invalid\n"] {
            let bytes = [
                prefix.clone(),
                frames(7, &[checkpoint(coverage)]),
                tail.to_vec(),
            ]
            .concat();
            let log = Log::new(&bytes);
            let boundary = log.boundary();
            if (2..=4).contains(&coverage) {
                let boundary = boundary.unwrap();
                assert_eq!(boundary.bytes, u64::try_from(prefix.len()).unwrap());
                assert_eq!(boundary.seq, 6);
            } else if tail.is_empty() {
                assert_eq!(boundary, Err(SessionError::SessionRecoveryNotNeeded));
            } else {
                assert_eq!(
                    boundary.unwrap().bytes,
                    u64::try_from(bytes.len() - tail.len()).unwrap()
                );
            }
        }
    }
}

#[test]
fn a_turn_open_at_its_checkpoint_is_copied_and_closed_as_failed() {
    let kept = frames(
        1,
        &[
            user("first"),
            assistant("done"),
            completed(),
            user("second"),
            call("one"),
            result("one"),
            checkpoint(6),
        ],
    );
    let bytes = [kept.clone(), frames(8, &[assistant("lost")]), b"{".to_vec()].concat();
    let source = Log::new(&bytes);
    let boundary = source.boundary().unwrap();
    assert!(boundary.turn_open);
    assert_eq!(boundary.seq, 7);

    let recovered = recovered_log(&source.dir, boundary).unwrap();
    assert_eq!(
        recovered.turns,
        [
            ArchivedTurn::Replied {
                user: "first".to_owned(),
                assistant: "done".to_owned(),
                execution: TurnExecution::default(),
            },
            ArchivedTurn::Compacted(CompactedHistory {
                summary: "checkpoint".to_owned(),
                removed_turn_count: 1,
                compaction_count: 1,
            }),
            ArchivedTurn::Interrupted {
                user: "second".to_owned(),
                assistant: None,
                tool_call: None,
                completed_tool_names: Vec::new(),
                execution: TurnExecution {
                    steps: vec![ExecutedStep {
                        assistant: None,
                        calls: vec![ToolCallEvent::new(
                            "one",
                            "shell",
                            "{}",
                            ToolArgumentIntegrity::Valid,
                        )],
                        results: vec![ArchivedResult::from_event(ToolResultEvent::new(
                            "one",
                            "shell",
                            ToolResultStatus::Success,
                            "result-one.txt",
                            0,
                            ArtifactCompleteness::Complete,
                        ))],
                    }],
                    ..TurnExecution::default()
                },
            },
        ]
    );
    assert_eq!(
        recovered.artifacts,
        [RecoveredArtifact::ToolOutput {
            handle: "result-one.txt".to_owned(),
            bytes: 0,
        }]
    );

    let target = Log::new(b"");
    fs::remove_file(target.root.path().join("session").join(EVENTS_FILE)).unwrap();
    copy_conversation_recovery_prefix(&source.dir, &target.dir, boundary).unwrap();
    let mut closing = kept;
    closing.extend(encode_conversation_frame(8, 1, &interrupted()).unwrap());
    assert_eq!(target.bytes(), closing);
    assert_eq!(source.bytes(), bytes);
}

#[test]
fn an_unreadable_usage_sidecar_needs_recovery_even_for_a_complete_log() {
    let log = Log::new(&frames(1, &[user("fact"), assistant("saved"), completed()]));
    log.dir.replace("usage-v2.json", b"").unwrap();
    let recovery = classify_conversation_recovery(&log.dir, "session").unwrap();
    assert!(recovery.usage_incomplete);
    assert_eq!(recovery.boundary.seq, 3);
}
