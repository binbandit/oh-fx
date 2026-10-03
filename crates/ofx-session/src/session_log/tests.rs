use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use ofx_config::ProviderId;
use ofx_contract::{ReasoningEffort, ToolArgumentIntegrity, ToolResultStatus};

use super::*;
use crate::session_codec::SavedProvider;
use crate::session_event::{
    ArtifactCompleteness, AssistantEvent, ContextCheckpointEvent, ToolCallEvent, ToolResultEvent,
    TurnCompletedEvent, UserEvent,
};

struct Fixture {
    root: tempfile::TempDir,
    sessions: PrivateDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let data = PrivateDir::open_or_create(&root.path().join("data/oh-fx")).unwrap();
        let sessions = data.open_or_create_child("sessions").unwrap();
        Self { root, sessions }
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.root.path().join("data/oh-fx/sessions").join(id)
    }

    fn events(&self, id: &str) -> PathBuf {
        self.dir(id).join(EVENTS_FILE)
    }

    fn start(&self, id: &str) -> WritableSession {
        start_session(&self.sessions, metadata(id)).unwrap()
    }

    fn resume(&self, id: &str) -> Result<WritableSession, SessionError> {
        resume_session(&self.sessions, id, LOCK_DEADLINE)
    }

    fn append_raw(&self, id: &str, bytes: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(self.events(id))
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(self.root.path().join("data/oh-fx/sessions"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

fn metadata(id: &str) -> SessionMetadata {
    SessionMetadata {
        id: id.to_owned(),
        origin_workspace_root: "/workspace".to_owned(),
        workspace_root: "/workspace".to_owned(),
        created_at_ms: 1,
        updated_at_ms: 1,
        conversation_language: "und".to_owned(),
        preferences: SessionPreferences {
            provider: SavedProvider::new(ProviderId::Gateway, None).unwrap(),
            model: "openai/gpt-5".to_owned(),
            effort: ReasoningEffort::Auto,
            fast_mode: false,
        },
        title: None,
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

fn call(id: &str) -> ConversationEvent {
    ConversationEvent::ToolCall(ToolCallEvent::new(
        id,
        "shell",
        "{\"command\":\"ls\"}",
        ToolArgumentIntegrity::Valid,
    ))
}

fn result(id: &str) -> ConversationEvent {
    ConversationEvent::ToolResult(ToolResultEvent::new(
        id,
        "shell",
        ToolResultStatus::Success,
        "result.txt",
        3,
        ArtifactCompleteness::Complete,
    ))
}

fn completed() -> ConversationEvent {
    ConversationEvent::TurnCompleted(TurnCompletedEvent::default())
}

fn checkpoint(covers_through_seq: u64, summary: &str) -> ConversationEvent {
    ConversationEvent::ContextCheckpoint(ContextCheckpointEvent {
        covers_through_seq,
        summary: summary.to_owned(),
    })
}

fn interrupted() -> ConversationEvent {
    ConversationEvent::Interrupted(InterruptedEvent::new(InterruptReason::Failed, None))
}

fn turn(prompt: &str) -> Vec<ConversationEvent> {
    vec![
        user(prompt),
        assistant(&format!("re: {prompt}")),
        completed(),
    ]
}

fn tool_turn(prompt: &str, id: &str) -> Vec<ConversationEvent> {
    vec![
        user(prompt),
        assistant(""),
        call(id),
        result(id),
        assistant("done"),
        completed(),
    ]
}

fn frame(seq: u64, event: &ConversationEvent) -> Vec<u8> {
    crate::session_event::encode_conversation_frame(seq, 5, event).unwrap()
}

fn prompts(history: &SavedHistory) -> Vec<String> {
    history
        .turns
        .iter()
        .map(|turn| match &turn.events[0] {
            ConversationEvent::User(user) => user.text.clone(),
            other => panic!("turn starts with {other:?}"),
        })
        .collect()
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

fn line_count(path: &Path) -> usize {
    fs::read_to_string(path).unwrap().lines().count()
}

#[test]
fn conversation_storage_creates_only_private_metadata_and_event_log() {
    let fixture = Fixture::new();
    let session = fixture.start("fresh");
    assert_eq!(session.id(), "fresh");
    assert!(!session.previous_owner_died());
    assert_eq!(fixture.names(), ["fresh"]);
    let dir = fixture.dir("fresh");
    let mut files: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(
        files,
        ["events.jsonl", "owner.live", "session.json", "session.lock"]
    );
    assert_eq!(mode(&dir), 0o700);
    for file in &files {
        assert_eq!(mode(&dir.join(file)), 0o600, "{file}");
    }
    assert_eq!(fs::read(dir.join("events.jsonl")).unwrap(), b"");
    assert_eq!(
        decode_session_metadata(&fs::read(dir.join("session.json")).unwrap()).unwrap(),
        metadata("fresh")
    );
    drop(session);
    assert!(!dir.join("owner.live").exists());
}

#[test]
fn session_creation_never_replaces_an_existing_session() {
    let fixture = Fixture::new();
    drop(fixture.start("taken"));
    assert_eq!(
        start_session(&fixture.sessions, metadata("taken")).err(),
        Some(SessionError::SessionAlreadyExists)
    );
    fs::create_dir(fixture.dir("racing")).unwrap();
    assert_eq!(
        start_session(&fixture.sessions, metadata("racing")).err(),
        Some(SessionError::SessionAlreadyExists)
    );
    let mut invalid = metadata("bad");
    invalid.preferences.model = String::new();
    assert_eq!(
        start_session(&fixture.sessions, invalid).err(),
        Some(SessionError::InvalidDurableField)
    );
    assert_eq!(fixture.names(), ["racing", "taken"]);
}

#[test]
fn publication_never_replaces_a_target_created_after_the_existence_check() {
    let fixture = Fixture::new();
    create_private_dir(&fixture.sessions, "creating+00").unwrap();
    fs::write(fixture.dir("creating+00").join(MANIFEST_FILE), "{}").unwrap();
    fs::create_dir(fixture.dir("racing")).unwrap();
    assert_eq!(
        publish_dir(&fixture.sessions, "creating+00", "racing"),
        Err(SessionError::SessionAlreadyExists)
    );
    assert_eq!(fs::read_dir(fixture.dir("racing")).unwrap().count(), 0);
    remove_created_dir(&fixture.sessions, "creating+00");
    assert_eq!(fixture.names(), ["racing"]);
}

#[test]
fn conversation_writer_appends_one_durable_line_per_event() {
    let fixture = Fixture::new();
    let mut session = fixture.start("lines");
    session.append(10, &tool_turn("inspect", "call-1")).unwrap();
    assert_eq!(session.metadata().updated_at_ms, 10);
    session.append(11, &turn("second")).unwrap();
    assert_eq!(line_count(&fixture.events("lines")), 9);
    let bytes = fs::read(fixture.events("lines")).unwrap();
    let first = bytes.split(|byte| *byte == b'\n').next().unwrap();
    assert_eq!(
        first,
        b"{\"schema_version\":3,\"seq\":1,\"timestamp_ms\":10,\"event\":{\"user\":{\"text\":\"inspect\",\"images\":[],\"work_id\":null}}}"
    );
    drop(session);
    let loaded = load_session(&fixture.sessions, "lines").unwrap();
    assert_eq!(prompts(&loaded.history), ["inspect", "second"]);
    assert_eq!(
        loaded.history.turns[0].events,
        tool_turn("inspect", "call-1")
    );
    assert!(loaded.history.compacted.is_none());
}

#[test]
fn appends_validate_whole_batches_before_writing() {
    let fixture = Fixture::new();
    let mut session = fixture.start("batches");
    assert_eq!(
        session.append(1, &[user("x"), call("c")]),
        Err(SessionError::UnresolvedToolCall)
    );
    assert_eq!(
        session.append(1, &[assistant("x")]),
        Err(SessionError::InvalidConversationFrame)
    );
    assert_eq!(
        session.append(1, &[user("x"), completed(), user("y"), completed()]),
        Err(SessionError::InvalidConversationEvent)
    );
    assert_eq!(
        session.append(1, &[user("x"), checkpoint(0, "early"), completed()]),
        Err(SessionError::InvalidConversationEvent)
    );
    assert_eq!(line_count(&fixture.events("batches")), 0);
    session.append(1, &[]).unwrap();
    session.append(1, &[user("open")]).unwrap();
    assert!(session.turn_open());
    session.append(2, &[assistant("a"), completed()]).unwrap();
    assert!(!session.turn_open());
    assert_eq!(line_count(&fixture.events("batches")), 3);
}

#[test]
fn conversation_writer_refuses_a_suffix_written_outside_its_ownership() {
    let fixture = Fixture::new();
    let mut session = fixture.start("foreign");
    session.append(1, &turn("one")).unwrap();
    let foreign = frame(4, &user("foreign"));
    fixture.append_raw("foreign", &foreign);
    assert_eq!(
        session.append(2, &turn("two")),
        Err(SessionError::SessionWriterChanged)
    );
    assert_eq!(
        session.append(2, &turn("two")),
        Err(SessionError::SessionWriterChanged)
    );
    assert_eq!(
        session.set_preferences(metadata("foreign").preferences, 3),
        Err(SessionError::SessionWriterChanged)
    );
    assert!(
        fs::read(fixture.events("foreign"))
            .unwrap()
            .ends_with(&foreign)
    );
}

#[test]
fn conversation_writer_rolls_back_failed_sync_and_refuses_uncertain_continuation() {
    let fixture = Fixture::new();
    let mut session = fixture.start("sync");
    session.append(1, &turn("kept")).unwrap();
    let committed = fs::metadata(fixture.events("sync")).unwrap().len();
    session.writer.fail_next_syncs(1);
    assert_eq!(
        session.append(2, &turn("rolled back")),
        Err(SessionError::Io(std::io::ErrorKind::Other))
    );
    assert_eq!(
        fs::metadata(fixture.events("sync")).unwrap().len(),
        committed
    );
    session.append(3, &turn("after")).unwrap();
    session.writer.fail_next_syncs(2);
    assert_eq!(
        session.append(4, &turn("uncertain")),
        Err(SessionError::SessionPersistenceUncertain)
    );
    assert_eq!(
        session.append(5, &turn("refused")),
        Err(SessionError::SessionPersistenceUncertain)
    );
    drop(session);
    let mut resumed = fixture.resume("sync").unwrap();
    assert_eq!(prompts(&resumed.take_history()), ["kept", "after"]);
}

#[test]
fn owner_liveness_marker_reports_unclean_exit_and_clears_on_clean_close() {
    let fixture = Fixture::new();
    drop(fixture.start("owner"));
    let resumed = fixture.resume("owner").unwrap();
    assert!(!resumed.previous_owner_died());
    let marker = fs::read_to_string(fixture.dir("owner").join("owner.live")).unwrap();
    assert!(marker.starts_with(&format!("{{\"pid\":{},\"opened_at_ms\":", process::id())));
    assert!(marker.ends_with("}\n"));
    drop(resumed);
    assert!(!fixture.dir("owner").join("owner.live").exists());
    fs::write(fixture.dir("owner").join("owner.live"), "{}\n").unwrap();
    fs::set_permissions(
        fixture.dir("owner").join("owner.live"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let after_crash = fixture.resume("owner").unwrap();
    assert!(after_crash.previous_owner_died());
}

#[test]
fn a_second_writer_waits_for_the_lock_and_reports_busy() {
    let fixture = Fixture::new();
    let held = fixture.start("busy");
    assert_eq!(
        resume_session(&fixture.sessions, "busy", Duration::ZERO).err(),
        Some(SessionError::SessionBusy)
    );
    assert!(fixture.dir("busy").join("owner.live").exists());
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        drop(held);
    });
    let resumed = fixture.resume("busy").unwrap();
    release.join().unwrap();
    assert!(!resumed.previous_owner_died());
}

#[test]
fn conversation_writer_repairs_only_a_partial_final_record_and_continues() {
    let fixture = Fixture::new();
    let mut session = fixture.start("torn");
    session.append(1, &turn("one")).unwrap();
    drop(session);
    let complete = fs::read(fixture.events("torn")).unwrap();
    let next = frame(4, &user("two"));
    fixture.append_raw("torn", &next[..next.len() / 2]);
    let loaded = load_session(&fixture.sessions, "torn").unwrap();
    assert_eq!(prompts(&loaded.history), ["one"]);
    assert_ne!(fs::read(fixture.events("torn")).unwrap(), complete);
    let mut resumed = fixture.resume("torn").unwrap();
    assert_eq!(fs::read(fixture.events("torn")).unwrap(), complete);
    assert_eq!(prompts(&resumed.take_history()), ["one"]);
    resumed.append(2, &turn("two")).unwrap();
    drop(resumed);
    let reloaded = load_session(&fixture.sessions, "torn").unwrap();
    assert_eq!(prompts(&reloaded.history), ["one", "two"]);
    assert_eq!(line_count(&fixture.events("torn")), 6);
}

#[test]
fn conversation_writer_truncates_a_complete_record_partial_turn() {
    let fixture = Fixture::new();
    let mut session = fixture.start("partial");
    session.append(1, &turn("one")).unwrap();
    drop(session);
    let complete = fs::read(fixture.events("partial")).unwrap();
    fixture.append_raw("partial", &frame(4, &user("two")));
    fixture.append_raw("partial", &frame(5, &assistant("half")));
    fixture.append_raw("partial", &frame(6, &call("c")));
    let loaded = load_session(&fixture.sessions, "partial").unwrap();
    assert_eq!(prompts(&loaded.history), ["one"]);
    let mut resumed = fixture.resume("partial").unwrap();
    assert!(!resumed.turn_open());
    assert_eq!(fs::read(fixture.events("partial")).unwrap(), complete);
    assert_eq!(prompts(&resumed.take_history()), ["one"]);
    resumed.append(2, &turn("retry")).unwrap();
    drop(resumed);
    let lines = fs::read_to_string(fixture.events("partial")).unwrap();
    assert!(lines.lines().nth(3).unwrap().contains("\"seq\":4,"));
}

#[test]
fn conversation_writer_removes_an_unfinished_turn_before_a_torn_final_record() {
    let fixture = Fixture::new();
    let mut session = fixture.start("both");
    session.append(1, &turn("one")).unwrap();
    drop(session);
    let complete = fs::read(fixture.events("both")).unwrap();
    fixture.append_raw("both", &frame(4, &user("two")));
    let torn = frame(5, &assistant("never finished"));
    fixture.append_raw("both", &torn[..torn.len() - 1]);
    drop(fixture.resume("both").unwrap());
    assert_eq!(fs::read(fixture.events("both")).unwrap(), complete);
}

#[test]
fn a_checkpoint_never_covers_an_answered_call_without_its_result() {
    let fixture = Fixture::new();
    let mut session = fixture.start("split");
    session
        .append(1, &[user("inspect"), call("c1"), result("c1"), completed()])
        .unwrap();
    let before = fs::read(fixture.events("split")).unwrap();
    assert_eq!(
        session.append(2, &[checkpoint(2, "covers the call only")]),
        Err(SessionError::InvalidCheckpointCoverage)
    );
    assert_eq!(fs::read(fixture.events("split")).unwrap(), before);
    session
        .append(
            3,
            &[
                user("two steps"),
                assistant(""),
                call("c2"),
                result("c2"),
                assistant(""),
                call("c3"),
                result("c3"),
                assistant("done"),
                completed(),
            ],
        )
        .unwrap();
    assert_eq!(
        session.append(4, &[checkpoint(10, "covers half of the second step")]),
        Err(SessionError::InvalidCheckpointCoverage)
    );
    session
        .append(4, &[checkpoint(8, "covers the first step")])
        .unwrap();
    drop(session);
    let loaded = load_session(&fixture.sessions, "split").unwrap();
    assert_eq!(loaded.history.compacted.unwrap().removed_turn_count, 1);
    assert_eq!(
        loaded.history.turns[0].events,
        [
            user("two steps"),
            assistant(""),
            call("c3"),
            result("c3"),
            assistant("done"),
            completed(),
        ]
    );

    drop(fixture.start("forged"));
    let forged = [
        frame(1, &user("inspect")),
        frame(2, &call("c1")),
        frame(3, &result("c1")),
        frame(4, &completed()),
        frame(5, &checkpoint(2, "covers the call only")),
    ]
    .concat();
    fixture.append_raw("forged", &forged);
    assert_eq!(
        load_session(&fixture.sessions, "forged").err(),
        Some(SessionError::InvalidCheckpointCoverage)
    );
    assert_eq!(
        fixture.resume("forged").err(),
        Some(SessionError::InvalidCheckpointCoverage)
    );
    assert_eq!(fs::read(fixture.events("forged")).unwrap(), forged);
}

#[test]
fn replay_refuses_a_result_whose_call_it_did_not_restore() {
    let events = [
        user("inspect"),
        call("c1"),
        result("c1"),
        completed(),
        checkpoint(2, "covers the call only"),
    ];
    let mut file = tempfile::tempfile().unwrap();
    let mut replay = ReplayScan::default();
    let mut end = 0;
    for (seq, event) in (1..).zip(&events) {
        let bytes = frame(seq, event);
        file.write_all(&bytes).unwrap();
        replay.observe(end, seq, event).unwrap();
        end += u64::try_from(bytes.len()).unwrap();
    }
    let window = replay.finish(&file, end).unwrap();
    assert_eq!(
        replay_history(&file, end, &window),
        Err(SessionError::InvalidConversationFrame)
    );
}

#[test]
fn a_mid_turn_checkpoint_survives_restart_and_closes_its_turn() {
    let fixture = Fixture::new();
    let mut session = fixture.start("mid");
    session.append(1, &turn("before")).unwrap();
    session
        .append(
            2,
            &[
                user("long task"),
                assistant(""),
                call("c1"),
                result("c1"),
                checkpoint(7, "<summary>before and the first call</summary>"),
            ],
        )
        .unwrap();
    assert!(session.turn_open());
    drop(session);
    fixture.append_raw("mid", &frame(9, &assistant("lost after the checkpoint")));
    let mut resumed = fixture.resume("mid").unwrap();
    assert!(!resumed.turn_open());
    let history = resumed.take_history();
    assert_eq!(
        history.compacted,
        Some(CompactedHistory {
            summary: "<summary>before and the first call</summary>".to_owned(),
            removed_turn_count: 1,
            compaction_count: 1,
        })
    );
    assert_eq!(history.turns.len(), 1);
    let events = &history.turns[0].events;
    assert_eq!(events[0], user("long task"));
    assert!(matches!(events[1], ConversationEvent::ContextCheckpoint(_)));
    assert!(matches!(
        &events[2],
        ConversationEvent::Interrupted(interrupted) if interrupted.reason == InterruptReason::Failed
    ));
    assert_eq!(events.len(), 3);
    let text = fs::read_to_string(fixture.events("mid")).unwrap();
    assert!(!text.contains("lost after the checkpoint"));
    assert!(text.lines().last().unwrap().contains("\"seq\":9,"));
}

#[test]
fn replay_starts_after_the_latest_checkpoint_and_keeps_retained_turns() {
    let fixture = Fixture::new();
    let mut session = fixture.start("windows");
    session.append(1, &turn("one")).unwrap();
    session.append(2, &turn("two")).unwrap();
    session
        .append(3, &[checkpoint(6, "first summary")])
        .unwrap();
    session.append(4, &turn("three")).unwrap();
    session.append(5, &turn("four")).unwrap();
    session
        .append(6, &[checkpoint(10, "second summary")])
        .unwrap();
    session.append(7, &turn("five")).unwrap();
    drop(session);
    let loaded = load_session(&fixture.sessions, "windows").unwrap();
    assert_eq!(
        loaded.history.compacted,
        Some(CompactedHistory {
            summary: "second summary".to_owned(),
            removed_turn_count: 3,
            compaction_count: 2,
        })
    );
    assert_eq!(prompts(&loaded.history), ["four", "five"]);
    let mut resumed = fixture.resume("windows").unwrap();
    assert_eq!(resumed.take_history(), loaded.history);
}

#[test]
fn the_transcript_walks_every_saved_turn_including_compacted_ones() {
    let fixture = Fixture::new();
    let mut session = fixture.start("transcript");
    session.append(1, &turn("one")).unwrap();
    session
        .append(2, &[checkpoint(3, "first summary")])
        .unwrap();
    session.append(3, &tool_turn("two", "c1")).unwrap();
    session
        .append(4, &[user("three"), assistant(""), call("c2"), result("c2")])
        .unwrap();
    session
        .append(5, &[checkpoint(14, "second summary")])
        .unwrap();
    drop(session);
    let mut resumed = fixture.resume("transcript").unwrap();
    let mut walked = Vec::new();
    resumed.visit_transcript(|turn| walked.push(turn)).unwrap();
    let walked = SavedHistory {
        compacted: None,
        turns: walked,
    };
    assert_eq!(prompts(&walked), ["one", "two", "three"]);
    assert_eq!(walked.turns[1].events, tool_turn("two", "c1"));
    let open = &walked.turns[2].events;
    assert_eq!(open.len(), 6);
    assert!(matches!(open[4], ConversationEvent::ContextCheckpoint(_)));
    assert!(matches!(open[5], ConversationEvent::Interrupted(_)));
    assert_eq!(prompts(&resumed.take_history()), ["three"]);
}

#[test]
fn the_display_title_prefers_the_saved_title_over_the_first_prompt() {
    let fixture = Fixture::new();
    let mut session = fixture.start("titled");
    session.append(1, &turn("/help")).unwrap();
    session.append(2, &turn("explain the build")).unwrap();
    drop(session);
    let resumed = fixture.resume("titled").unwrap();
    assert_eq!(resumed.display_title(), "explain the build");
    let mut titled = metadata("named");
    titled.title = Some("Release prep".to_owned());
    let named = start_session(&fixture.sessions, titled).unwrap();
    assert_eq!(named.display_title(), "Release prep");
}

fn saved_metadata(fixture: &Fixture, id: &str) -> SessionMetadata {
    decode_session_metadata(&fs::read(fixture.dir(id).join(MANIFEST_FILE)).unwrap()).unwrap()
}

#[test]
fn a_rename_saves_only_the_title_and_later_metadata_writes_keep_it() {
    let fixture = Fixture::new();
    let mut session = fixture.start("renamed");
    assert_eq!(session.title(), None);
    session.rename("deploy pipeline fix").unwrap();
    assert_eq!(session.title(), Some("deploy pipeline fix"));
    let mut expected = metadata("renamed");
    expected.title = Some("deploy pipeline fix".to_owned());
    assert_eq!(saved_metadata(&fixture, "renamed"), expected);
    let mut preferences = expected.preferences.clone();
    preferences.fast_mode = true;
    session.set_preferences(preferences, 5).unwrap();
    assert_eq!(
        saved_metadata(&fixture, "renamed").title.as_deref(),
        Some("deploy pipeline fix")
    );
    session.append(6, &turn("ship it")).unwrap();
    drop(session);
    let resumed = fixture.resume("renamed").unwrap();
    assert_eq!(resumed.title(), Some("deploy pipeline fix"));
    assert_eq!(resumed.display_title(), "deploy pipeline fix");
}

#[test]
fn a_generated_title_never_replaces_one_the_user_chose() {
    let fixture = Fixture::new();
    let mut untitled = fixture.start("untitled");
    assert_eq!(
        untitled.install_generated_title("Fix the renderer"),
        Ok(true)
    );
    assert_eq!(
        saved_metadata(&fixture, "untitled").title.as_deref(),
        Some("Fix the renderer")
    );

    let mut derived = fixture.start("derived");
    derived.append(1, &turn("/help")).unwrap();
    derived
        .append(2, &turn("fix the flaky renderer test please now"))
        .unwrap();
    derived
        .rename("fix the flaky renderer test please now")
        .unwrap();
    assert_eq!(
        derived.install_generated_title("Flaky renderer test"),
        Ok(true)
    );
    assert_eq!(derived.title(), Some("Flaky renderer test"));

    let mut chosen = fixture.start("chosen");
    chosen.append(1, &turn("fix the renderer")).unwrap();
    chosen.rename("Release prep").unwrap();
    assert_eq!(
        chosen.install_generated_title("Fix the renderer"),
        Ok(false)
    );
    assert_eq!(
        saved_metadata(&fixture, "chosen").title.as_deref(),
        Some("Release prep")
    );

    let mut named_early = fixture.start("named-early");
    named_early.rename("Untitled session").unwrap();
    assert_eq!(named_early.install_generated_title("Generated"), Ok(false));
    assert_eq!(named_early.title(), Some("Untitled session"));
}

#[test]
fn checkpoint_only_sessions_replay_their_summary() {
    let fixture = Fixture::new();
    let mut session = fixture.start("summary");
    session.append(1, &turn("one")).unwrap();
    session.append(2, &[checkpoint(3, "everything")]).unwrap();
    drop(session);
    let loaded = load_session(&fixture.sessions, "summary").unwrap();
    assert!(loaded.history.turns.is_empty());
    assert_eq!(loaded.history.compacted.unwrap().removed_turn_count, 1);
}

#[test]
fn corrupt_records_fail_reads_and_resumes_without_changing_the_log() {
    let fixture = Fixture::new();
    let mut session = fixture.start("corrupt");
    session.append(1, &turn("one")).unwrap();
    drop(session);
    fixture.append_raw("corrupt", b"{\"not\":\"a frame\"}\n");
    fixture.append_raw("corrupt", &frame(4, &user("two")));
    let before = fs::read(fixture.events("corrupt")).unwrap();
    assert_eq!(
        load_session(&fixture.sessions, "corrupt").err(),
        Some(SessionError::InvalidConversationFrame)
    );
    assert_eq!(
        fixture.resume("corrupt").err(),
        Some(SessionError::InvalidConversationFrame)
    );
    assert_eq!(fs::read(fixture.events("corrupt")).unwrap(), before);
    assert!(!fixture.dir("corrupt").join("owner.live").exists());
}

#[test]
fn out_of_order_and_orphan_records_are_rejected_on_replay() {
    let fixture = Fixture::new();
    drop(fixture.start("gap"));
    fixture.append_raw("gap", &frame(2, &user("skipped one")));
    assert_eq!(
        load_session(&fixture.sessions, "gap").err(),
        Some(SessionError::OutOfOrderConversationEvent)
    );
    drop(fixture.start("orphan"));
    fixture.append_raw("orphan", &frame(1, &user("x")));
    fixture.append_raw("orphan", &frame(2, &result("never-called")));
    assert_eq!(
        fixture.resume("orphan").err(),
        Some(SessionError::OrphanToolResult)
    );
}

#[test]
fn unsafe_session_entries_are_refused() {
    let fixture = Fixture::new();
    drop(fixture.start("real"));
    symlink(fixture.dir("real"), fixture.dir("linked")).unwrap();
    assert_eq!(
        fixture.resume("linked").err(),
        Some(SessionError::SessionPathUnsafe)
    );
    assert_eq!(
        load_session(&fixture.sessions, "linked").err(),
        Some(SessionError::SessionPathUnsafe)
    );
    assert_eq!(
        load_session(&fixture.sessions, "../real").err(),
        Some(SessionError::InvalidSessionId)
    );
    assert_eq!(
        fixture.resume("missing").err(),
        Some(SessionError::SessionNotFound)
    );

    drop(fixture.start("symlinked-log"));
    let outside = fixture.root.path().join("outside.jsonl");
    fs::write(&outside, b"").unwrap();
    fs::remove_file(fixture.events("symlinked-log")).unwrap();
    symlink(&outside, fixture.events("symlinked-log")).unwrap();
    assert_eq!(
        fixture.resume("symlinked-log").err(),
        Some(SessionError::SessionPathUnsafe)
    );

    drop(fixture.start("hard-linked"));
    fs::hard_link(
        fixture.events("hard-linked"),
        fixture.root.path().join("second-name"),
    )
    .unwrap();
    assert_eq!(
        load_session(&fixture.sessions, "hard-linked").err(),
        Some(SessionError::SessionPathUnsafe)
    );

    drop(fixture.start("shared"));
    fs::set_permissions(fixture.events("shared"), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(load_session(&fixture.sessions, "shared").is_ok());
    assert_eq!(
        fixture.resume("shared").err(),
        Some(SessionError::PrivateStatePermissionsUnsupported)
    );

    drop(fixture.start("fifo"));
    fs::remove_file(fixture.events("fifo")).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(fixture.events("fifo"))
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        load_session(&fixture.sessions, "fifo").err(),
        Some(SessionError::SessionPathUnsafe)
    );
}

#[test]
fn writable_opens_restore_private_directory_permissions() {
    let fixture = Fixture::new();
    drop(fixture.start("loose"));
    fs::set_permissions(fixture.dir("loose"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(load_session(&fixture.sessions, "loose").is_ok());
    assert_eq!(mode(&fixture.dir("loose")), 0o755);
    drop(fixture.resume("loose").unwrap());
    assert_eq!(mode(&fixture.dir("loose")), 0o700);
}

#[test]
fn metadata_must_name_its_own_directory() {
    let fixture = Fixture::new();
    drop(fixture.start("named"));
    let renamed = fixture.dir("renamed");
    fs::rename(fixture.dir("named"), &renamed).unwrap();
    assert_eq!(
        load_session(&fixture.sessions, "renamed").err(),
        Some(SessionError::InvalidSessionMetadata)
    );
    fs::write(
        renamed.join("session.json"),
        vec![b' '; MAX_SESSION_METADATA_BYTES + 1],
    )
    .unwrap();
    assert_eq!(
        load_session(&fixture.sessions, "renamed").err(),
        Some(SessionError::InvalidSessionFormat)
    );
    fs::remove_file(renamed.join("session.json")).unwrap();
    assert_eq!(
        load_session(&fixture.sessions, "renamed").err(),
        Some(SessionError::SessionNotFound)
    );
}

#[test]
fn preference_changes_rewrite_metadata_durably() {
    let fixture = Fixture::new();
    let mut session = fixture.start("prefs");
    let preferences = SessionPreferences {
        provider: SavedProvider::new(ProviderId::Codex, None).unwrap(),
        model: "gpt-5.4".to_owned(),
        effort: ReasoningEffort::Named("high".to_owned()),
        fast_mode: true,
    };
    session.set_preferences(preferences.clone(), 50).unwrap();
    assert_eq!(session.metadata().preferences, preferences);
    let mut invalid = preferences.clone();
    invalid.model = " padded".to_owned();
    assert_eq!(
        session.set_preferences(invalid, 60),
        Err(SessionError::InvalidDurableField)
    );
    drop(session);
    let loaded = load_session(&fixture.sessions, "prefs").unwrap();
    assert_eq!(loaded.metadata.preferences, preferences);
    assert_eq!(loaded.metadata.updated_at_ms, 50);
    let leftovers: Vec<_> = fs::read_dir(fixture.dir("prefs"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.contains(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn a_session_whose_turns_carry_upstream_file_evidence_loads_and_resumes() {
    let fixture = Fixture::new();
    drop(fixture.start("evidence"));
    let frames = [
        "{\"schema_version\":3,\"seq\":1,\"timestamp_ms\":2,\"event\":{\"user\":{\"text\":\"read it\",\"images\":[],\"work_id\":null}}}\n",
        "{\"schema_version\":3,\"seq\":2,\"timestamp_ms\":2,\"event\":{\"assistant\":{\"text\":\"done\",\"provider_replay\":null,\"standalone_response\":false}}}\n",
        "{\"schema_version\":3,\"seq\":3,\"timestamp_ms\":2,\"event\":{\"turn_completed\":{\"files\":[{\"path\":\"src/main.rs\",\"new_path\":null,\"tool_call_id\":\"call_1\",\"tool_name\":\"read_file\",\"action\":\"read\",\"status\":\"success\",\"model_view_covers_full_file\":true,\"stale\":false}],\"turn_summary\":null}}}\n",
    ];
    for frame in frames {
        fixture.append_raw("evidence", frame.as_bytes());
    }
    let loaded = load_session(&fixture.sessions, "evidence").unwrap();
    assert_eq!(loaded.history.turns.len(), 1);
    let mut resumed = fixture.resume("evidence").unwrap();
    assert_eq!(resumed.take_history().turns.len(), 1);
    resumed.append(3, &[user("next"), completed()]).unwrap();
    drop(resumed);
    let log = fs::read_to_string(fixture.events("evidence")).unwrap();
    assert!(log.starts_with(&frames.concat()), "{log}");
    assert_eq!(log.lines().count(), 5);
}

#[test]
fn a_model_choice_saves_its_fast_mode_and_keeps_the_other_preferences() {
    let fixture = Fixture::new();
    let mut session = fixture.start("chosen");
    session
        .select_model("openai/gpt-5-mini", None, true)
        .unwrap();
    let loaded = load_session(&fixture.sessions, "chosen").unwrap();
    let mut expected = metadata("chosen").preferences;
    expected.model = "openai/gpt-5-mini".to_owned();
    expected.fast_mode = true;
    assert_eq!(loaded.metadata.preferences, expected);
    let picked = ReasoningEffort::Named("xhigh".to_owned());
    session
        .select_model("openai/gpt-5", Some(&picked), false)
        .unwrap();
    drop(session);
    let loaded = load_session(&fixture.sessions, "chosen").unwrap();
    expected.model = "openai/gpt-5".to_owned();
    expected.effort = picked;
    expected.fast_mode = false;
    assert_eq!(loaded.metadata.preferences, expected);
}

#[test]
fn a_batch_cut_anywhere_by_a_crash_is_removed_as_a_whole() {
    let fixture = Fixture::new();
    let mut session = fixture.start("atomic");
    session.append(1, &turn("kept")).unwrap();
    session
        .append(
            2,
            &[
                user("long"),
                assistant(""),
                call("c1"),
                result("c1"),
                checkpoint(7, "s"),
            ],
        )
        .unwrap();
    drop(session);
    let durable = fs::read(fixture.events("atomic")).unwrap();
    let batches = [
        vec![
            frame(9, &assistant("")),
            frame(10, &call("c2")),
            frame(11, &result("c2")),
            frame(12, &completed()),
        ],
        vec![
            frame(9, &assistant("partial answer")),
            frame(10, &completed()),
        ],
    ];
    for batch in batches {
        let bytes = batch.concat();
        let cuts = (1..bytes.len())
            .filter(|cut| cut % 7 == 0 || bytes[cut - 1] == b'\n' || bytes[*cut] == b'\n');
        for cut in cuts {
            fs::write(
                fixture.events("atomic"),
                [&durable[..], &bytes[..cut]].concat(),
            )
            .unwrap();
            let loaded = load_session(&fixture.sessions, "atomic").unwrap();
            assert!(loaded.history.turns.is_empty(), "cut at {cut}");
            assert_eq!(loaded.history.compacted.unwrap().removed_turn_count, 1);
            let mut resumed = fixture.resume("atomic").unwrap();
            let history = resumed.take_history();
            assert_eq!(history.turns.len(), 1, "cut at {cut}");
            assert_eq!(
                history.turns[0].events.last(),
                Some(&interrupted()),
                "cut at {cut}"
            );
            drop(resumed);
            let repaired = fs::read(fixture.events("atomic")).unwrap();
            assert!(repaired.starts_with(&durable), "cut at {cut}");
            let suffix = String::from_utf8(repaired[durable.len()..].to_vec()).unwrap();
            assert_eq!(suffix.lines().count(), 1, "cut at {cut}: {suffix}");
            assert!(
                suffix.contains("\"seq\":9,") && suffix.contains("\"interrupted\""),
                "{suffix}"
            );
        }
    }
}

fn replied_turn(user: &str) -> HistoryTurn<'_> {
    HistoryTurn {
        user,
        steps: Vec::new(),
        steering: Vec::new(),
        end: TurnEnd::Replied {
            text: "ok",
            provider_replay: None,
        },
    }
}

fn saved_language(fixture: &Fixture, id: &str) -> String {
    let manifest = fs::read(fixture.dir(id).join(MANIFEST_FILE)).unwrap();
    decode_session_metadata(&manifest)
        .unwrap()
        .conversation_language
}

#[test]
fn a_committed_turn_saves_the_language_of_the_latest_prompt() {
    let fixture = Fixture::new();
    let provider = SavedProvider::new(ProviderId::Gateway, None).unwrap();
    let mut session = fixture.start("language");
    session.observe_prompt("12345 !!!");
    session
        .record_turn(&replied_turn("12345 !!!"), &provider)
        .unwrap();
    assert_eq!(saved_language(&fixture, "language"), "und");
    session.observe_prompt("Проверь файл, please");
    let before = fs::read(fixture.dir("language").join(MANIFEST_FILE)).unwrap();
    session.observe_prompt("12345 !!!");
    assert_eq!(
        fs::read(fixture.dir("language").join(MANIFEST_FILE)).unwrap(),
        before
    );
    session
        .record_turn(&replied_turn("12345 !!!"), &provider)
        .unwrap();
    assert_eq!(saved_language(&fixture, "language"), "und-Cyrl");
    assert_eq!(session.metadata().conversation_language, "und-Cyrl");
    let saved = fs::read(fixture.dir("language").join(MANIFEST_FILE)).unwrap();
    session.observe_prompt("ещё раз");
    session
        .record_turn(&replied_turn("ещё раз"), &provider)
        .unwrap();
    assert_eq!(
        fs::read(fixture.dir("language").join(MANIFEST_FILE)).unwrap(),
        saved
    );
    drop(session);
    let mut resumed = fixture.resume("language").unwrap();
    resumed.observe_prompt("42");
    resumed.record_turn(&replied_turn("42"), &provider).unwrap();
    assert_eq!(saved_language(&fixture, "language"), "und-Cyrl");
    resumed.observe_prompt("ランディングページを開いて");
    resumed
        .record_turn(&replied_turn("ランディングページを開いて"), &provider)
        .unwrap();
    assert_eq!(saved_language(&fixture, "language"), "ja");
}

#[test]
fn a_turn_left_unsaved_keeps_its_prompt_language_for_the_next_commit() {
    let fixture = Fixture::new();
    let provider = SavedProvider::new(ProviderId::Gateway, None).unwrap();
    let mut session = fixture.start("unsaved");
    session.observe_prompt("افتح الصفحة الرئيسية");
    session
        .record_turn(
            &HistoryTurn {
                user: "افتح الصفحة الرئيسية",
                steps: Vec::new(),
                steering: Vec::new(),
                end: TurnEnd::Stopped {
                    reason: TurnStop::Failed,
                    partial: "",
                },
            },
            &provider,
        )
        .unwrap();
    assert_eq!(saved_language(&fixture, "unsaved"), "und");
    session.observe_prompt("?");
    session.record_turn(&replied_turn("?"), &provider).unwrap();
    assert_eq!(saved_language(&fixture, "unsaved"), "und-Arab");
}
