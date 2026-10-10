use std::fs;
use std::os::unix::fs::PermissionsExt;

use ofx_config::ProviderId;
use ofx_contract::{FileEvidenceAction, ReasoningEffort, ToolArgumentIntegrity, ToolResultStatus};

use super::*;
use crate::session_codec::{
    SavedProvider, SessionMetadata, SessionPreferences, encode_session_metadata,
};
use crate::session_event::{
    ArtifactCompleteness, ContextCheckpointEvent, FileEvidence as SavedEvidence, InterruptReason,
    SteeringEvent, TurnCompletedEvent, UserEvent, encode_conversation_frame,
};

const ID: &str = "archived";

struct Store {
    root: tempfile::TempDir,
    session: PrivateDir,
}

impl Store {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let sessions = PrivateDir::open_or_create(&root.path().join("sessions")).unwrap();
        let session = sessions.open_or_create_child(ID).unwrap();
        session
            .replace(
                "session.json",
                &encode_session_metadata(&metadata(false)).unwrap(),
            )
            .unwrap();
        Self { root, session }
    }

    fn log(&self, events: &[ConversationEvent]) -> &Self {
        let mut bytes = Vec::new();
        for (seq, event) in (1..).zip(events) {
            bytes.extend(encode_conversation_frame(seq, 7, event).unwrap());
        }
        self.write(EVENTS_FILE, &bytes)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> &Self {
        self.session.replace(name, bytes).unwrap();
        self
    }

    fn archive(&self) -> Result<SessionArchive, SessionError> {
        load_archive(&self.session, ID, SessionError::SessionNotFound)
    }

    fn turns(&self, events: &[ConversationEvent]) -> Vec<ArchivedTurn> {
        self.log(events).archive().unwrap().turns
    }
}

fn metadata(subagent_child: bool) -> SessionMetadata {
    SessionMetadata {
        id: ID.to_owned(),
        origin_workspace_root: "/origin".to_owned(),
        workspace_root: "/workspace".to_owned(),
        created_at_ms: 3,
        updated_at_ms: 9,
        conversation_language: "es".to_owned(),
        preferences: SessionPreferences {
            provider: SavedProvider::new(ProviderId::Gateway, None).unwrap(),
            model: "openai/gpt-5".to_owned(),
            effort: ReasoningEffort::Auto,
            fast_mode: false,
            ultrafast_mode: false,
        },
        title: None,
        subagent_child,
    }
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

fn tool_call(id: &str) -> ToolCallEvent {
    ToolCallEvent::new(
        id,
        "shell",
        "{\"command\":\"ls\"}",
        ToolArgumentIntegrity::Valid,
    )
}

fn tool_result(id: &str) -> ToolResultEvent {
    let mut result = ToolResultEvent::new(
        id,
        "shell",
        ToolResultStatus::Success,
        "result.txt",
        3,
        ArtifactCompleteness::Complete,
    );
    result.preview = Some("a.txt".to_owned());
    result
}

fn evidence(path: &str) -> SavedEvidence {
    SavedEvidence {
        path: path.to_owned(),
        new_path: None,
        tool_call_id: "call-1".to_owned(),
        tool_name: "read_file".to_owned(),
        action: FileEvidenceAction::Read,
        status: ToolResultStatus::Success,
        model_view_covers_full_file: true,
        stale: false,
    }
}

fn completed() -> ConversationEvent {
    ConversationEvent::TurnCompleted(TurnCompletedEvent::default())
}

fn completed_with(files: Vec<SavedEvidence>) -> ConversationEvent {
    ConversationEvent::TurnCompleted(TurnCompletedEvent {
        files,
        turn_summary: None,
    })
}

fn checkpoint(covers_through_seq: u64, summary: &str) -> ConversationEvent {
    ConversationEvent::ContextCheckpoint(ContextCheckpointEvent {
        covers_through_seq,
        summary: summary.to_owned(),
    })
}

fn replied(user: &str, assistant: &str, execution: TurnExecution) -> ArchivedTurn {
    ArchivedTurn::Replied {
        user: user.to_owned(),
        assistant: assistant.to_owned(),
        execution,
    }
}

#[test]
fn an_archive_carries_the_saved_identity_times_and_language() {
    let archive = Store::new()
        .log(&[user("hola"), assistant("que tal"), completed()])
        .archive()
        .unwrap();
    assert_eq!(archive.id, ID);
    assert_eq!(archive.created_at_ms, 3);
    assert_eq!(archive.updated_at_ms, 9);
    assert_eq!(archive.conversation_language, "es");
    assert_eq!(archive.source, SessionSource::OhFx);
    assert_eq!(
        archive.turns,
        [replied("hola", "que tal", TurnExecution::default())]
    );
}

#[test]
fn tool_steps_keep_the_text_before_their_calls_and_the_turn_its_files() {
    let turns = Store::new().turns(&[
        user("list"),
        assistant("Listing."),
        ConversationEvent::ToolCall(tool_call("call-1")),
        ConversationEvent::ToolResult(tool_result("call-1")),
        assistant("done"),
        completed_with(vec![evidence("src/main.rs")]),
    ]);
    let execution = TurnExecution {
        steps: vec![ExecutedStep {
            assistant: Some("Listing.".to_owned()),
            calls: vec![tool_call("call-1")],
            results: vec![tool_result("call-1")],
        }],
        files: vec![evidence("src/main.rs").into()],
        steering: Vec::new(),
    };
    assert_eq!(turns, [replied("list", "done", execution)]);
}

#[test]
fn steering_takes_the_text_before_it_as_its_prefix() {
    let turns = Store::new().turns(&[
        user("start"),
        assistant("partial"),
        ConversationEvent::Steering(SteeringEvent {
            text: "also this".to_owned(),
        }),
        assistant("final"),
        completed(),
    ]);
    let execution = TurnExecution {
        steering: vec![ArchivedSteering {
            text: "also this".to_owned(),
            assistant_prefix: Some("partial".to_owned()),
            after_tool_step_count: 0,
        }],
        ..TurnExecution::default()
    };
    assert_eq!(turns, [replied("start", "final", execution)]);
}

#[test]
fn an_empty_reply_without_a_replay_leaves_no_step_and_one_with_text_stays() {
    let turns = Store::new().turns(&[
        user("question"),
        assistant(""),
        assistant("first"),
        assistant("answer"),
        completed(),
    ]);
    let execution = TurnExecution {
        steps: vec![ExecutedStep {
            assistant: Some("first".to_owned()),
            calls: Vec::new(),
            results: Vec::new(),
        }],
        ..TurnExecution::default()
    };
    assert_eq!(turns, [replied("question", "answer", execution)]);
}

#[test]
fn compactions_stay_in_place_and_count_the_turns_before_them() {
    let turns = Store::new().turns(&[
        user("one"),
        assistant("first"),
        completed(),
        checkpoint(3, "summary one"),
        user("two"),
        assistant("second"),
        completed(),
        checkpoint(7, "summary two"),
    ]);
    assert_eq!(
        turns,
        [
            replied("one", "first", TurnExecution::default()),
            ArchivedTurn::Compacted(CompactedHistory {
                summary: "summary one".to_owned(),
                removed_turn_count: 1,
                compaction_count: 1,
            }),
            replied("two", "second", TurnExecution::default()),
            ArchivedTurn::Compacted(CompactedHistory {
                summary: "summary two".to_owned(),
                removed_turn_count: 2,
                compaction_count: 2,
            }),
        ]
    );
}

#[test]
fn an_interruption_keeps_its_partial_text_and_the_one_call_it_cut_short() {
    let mut interrupted = InterruptedEvent::new(InterruptReason::Cancelled, Some("part".into()));
    interrupted.files = vec![evidence("src/lib.rs")];
    let turns = Store::new().turns(&[
        user("work"),
        assistant("working"),
        ConversationEvent::ToolCall(tool_call("call-1")),
        ConversationEvent::Interrupted(interrupted),
        user("open"),
    ]);
    assert_eq!(
        turns,
        [ArchivedTurn::Interrupted {
            user: "work".to_owned(),
            assistant: Some("part".to_owned()),
            tool_call: Some(tool_call("call-1")),
            execution: TurnExecution {
                files: vec![evidence("src/lib.rs").into()],
                ..TurnExecution::default()
            },
        }]
    );
}

#[test]
fn an_interruption_after_a_reply_keeps_that_reply_as_a_step() {
    let turns = Store::new().turns(&[
        user("work"),
        assistant("so far"),
        ConversationEvent::Interrupted(InterruptedEvent::new(InterruptReason::Failed, None)),
    ]);
    assert_eq!(
        turns,
        [ArchivedTurn::Interrupted {
            user: "work".to_owned(),
            assistant: None,
            tool_call: None,
            execution: TurnExecution {
                steps: vec![ExecutedStep {
                    assistant: Some("so far".to_owned()),
                    calls: Vec::new(),
                    results: Vec::new(),
                }],
                ..TurnExecution::default()
            },
        }]
    );
}

#[test]
fn a_session_that_cannot_be_shown_names_why() {
    let child = Store::new();
    child.log(&[]).write(
        "session.json",
        &encode_session_metadata(&metadata(true)).unwrap(),
    );
    assert_eq!(child.archive(), Err(SessionError::SessionNotFound));

    let missing_log = Store::new();
    assert_eq!(missing_log.archive(), Err(SessionError::SessionNotFound));

    let broken_log = Store::new();
    broken_log.write(EVENTS_FILE, b"{\"not\":\"a frame\"}\n");
    assert_eq!(broken_log.archive(), Err(SessionError::SessionNotFound));

    let linked_log = Store::new();
    linked_log.log(&[]);
    let log = linked_log
        .root
        .path()
        .join("sessions")
        .join(ID)
        .join(EVENTS_FILE);
    let moved = linked_log.root.path().join(EVENTS_FILE);
    fs::rename(&log, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &log).unwrap();
    assert_eq!(linked_log.archive(), Err(SessionError::SessionNotFound));
    assert_eq!(
        check_conversation(&linked_log.session, ID),
        Err(SessionError::SessionPathUnsafe)
    );

    let broken_metadata = Store::new();
    broken_metadata.log(&[]).write("session.json", b"{");
    assert_eq!(
        broken_metadata.archive(),
        Err(SessionError::InvalidSessionFormat)
    );

    let future = Store::new();
    future
        .log(&[])
        .write("session.json", b"{\"schema_version\":99}");
    assert_eq!(
        future.archive(),
        Err(SessionError::UnsupportedSessionSchema)
    );

    let unsafe_usage = Store::new();
    unsafe_usage.log(&[]).write("usage-v2.json", b"{}");
    fs::set_permissions(
        unsafe_usage
            .root
            .path()
            .join("sessions")
            .join(ID)
            .join("usage-v2.json"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_eq!(
        unsafe_usage.archive(),
        Err(SessionError::InvalidUsageSidecar)
    );

    let broken_recovery = Store::new();
    broken_recovery
        .log(&[user("open")])
        .write("recovery.json", b"{");
    assert_eq!(
        broken_recovery.archive(),
        Err(SessionError::InvalidRecoveryCheckpoint)
    );
}

#[test]
fn a_torn_last_line_is_left_out() {
    let store = Store::new();
    let mut bytes = Vec::new();
    for (seq, event) in (1..).zip([user("kept"), assistant("yes"), completed()]) {
        bytes.extend(encode_conversation_frame(seq, 7, &event).unwrap());
    }
    bytes.extend(b"{\"schema_version\":3,\"seq\":4");
    store.write(EVENTS_FILE, &bytes);
    assert_eq!(
        store.archive().unwrap().turns,
        [replied("kept", "yes", TurnExecution::default())]
    );
}
