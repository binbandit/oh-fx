use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use ofx_config::ProviderId;
use ofx_contract::ReasoningEffort;

use super::*;
use crate::session_codec::SavedProvider;
use crate::session_event::{
    AssistantEvent, ContextCheckpointEvent, ConversationEvent, TurnCompletedEvent, UserEvent,
};
use crate::session_log::start_session;

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn data(&self) -> PathBuf {
        self.root.path().join("data/oh-fx")
    }

    fn store(&self, workspace: &str) -> SessionStore {
        SessionStore::open(&self.data(), workspace).unwrap()
    }

    fn reader(&self, workspace: &str) -> SessionStore {
        SessionStore::open_read_only(&self.data(), workspace).unwrap()
    }

    fn session_dir(&self, id: &str) -> PathBuf {
        self.data().join("sessions").join(id)
    }

    fn seed(&self, id: &str, workspace: &str, turns: usize, modified_s: u64) {
        let store = self.store(workspace);
        let mut metadata = SessionMetadata {
            id: id.to_owned(),
            origin_workspace_root: workspace.to_owned(),
            workspace_root: workspace.to_owned(),
            created_at_ms: 1,
            updated_at_ms: 2,
            conversation_language: "en".to_owned(),
            preferences: preferences(),
            title: None,
        };
        if turns == 0 {
            metadata.updated_at_ms = i64::try_from(modified_s).unwrap() * 1000;
        }
        let mut session = start_session(store.sessions.as_ref().unwrap(), metadata).unwrap();
        for index in 0..turns {
            session.append(3, &turn(&format!("{id} {index}"))).unwrap();
        }
        drop(session);
        set_modified(&self.session_dir(id).join("events.jsonl"), modified_s);
    }
}

fn preferences() -> SessionPreferences {
    SessionPreferences {
        provider: SavedProvider::new(ProviderId::Gateway, None).unwrap(),
        model: "openai/gpt-5".to_owned(),
        effort: ReasoningEffort::Auto,
        fast_mode: false,
    }
}

fn turn(prompt: &str) -> Vec<ConversationEvent> {
    vec![
        ConversationEvent::User(UserEvent::new(prompt)),
        ConversationEvent::Assistant(AssistantEvent {
            text: "ok".to_owned(),
            provider_replay: None,
            standalone_response: false,
        }),
        ConversationEvent::TurnCompleted(TurnCompletedEvent::default()),
    ]
}

fn set_modified(path: &Path, seconds: u64) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(seconds))
        .unwrap();
}

fn ids(page: &ResumablePage) -> Vec<&str> {
    page.summaries
        .iter()
        .map(|summary| summary.id.as_str())
        .collect()
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn opening_a_store_creates_a_private_layout_and_reading_creates_nothing() {
    let fixture = Fixture::new();
    let reader = fixture.reader("/workspace");
    assert_eq!(
        reader.resumable_page(ListScope::AllWorkspaces, None, None, 10),
        Ok(ResumablePage::default())
    );
    assert_eq!(
        reader.load("anything").err(),
        Some(SessionError::SessionNotFound)
    );
    assert_eq!(
        reader.resume("anything").err(),
        Some(SessionError::SessionNotFound)
    );
    assert_eq!(
        reader.resume_latest().err(),
        Some(SessionError::SessionNotFound)
    );
    assert_eq!(reader.remembered_session_id(), Ok(None));
    assert_eq!(
        reader.start(preferences()).err(),
        Some(SessionError::SessionStoreUnavailable)
    );
    assert!(!fixture.root.path().join("data").exists());

    let store = fixture.store("/workspace//");
    assert_eq!(mode(&fixture.data()), 0o700);
    assert_eq!(mode(&fixture.data().join("sessions")), 0o700);
    let session = store.start(preferences()).unwrap();
    assert_eq!(session.metadata().workspace_root, "/workspace");
    assert_eq!(session.metadata().origin_workspace_root, "/workspace");
    assert_eq!(session.metadata().conversation_language, "und");
    assert_eq!(
        session.metadata().created_at_ms,
        session.metadata().updated_at_ms
    );
    assert!(is_valid_session_id(session.id()));
    assert_eq!(session.id().len(), 12);
    let names: Vec<_> = fs::read_dir(fixture.data().join("sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, [session.id()]);

    for invalid in ["", "relative", "./x"] {
        assert_eq!(
            SessionStore::open(&fixture.data(), invalid).err(),
            Some(SessionError::InvalidWorkspaceRoot)
        );
    }
}

#[test]
fn symlinked_profile_directories_are_refused() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.path().join("elsewhere")).unwrap();
    fs::create_dir_all(fixture.data()).unwrap();
    symlink(
        fixture.root.path().join("elsewhere"),
        fixture.data().join("sessions"),
    )
    .unwrap();
    assert_eq!(
        SessionStore::open(&fixture.data(), "/workspace").err(),
        Some(SessionError::SessionPathUnsafe)
    );
    assert_eq!(
        SessionStore::open_read_only(&fixture.data(), "/workspace").err(),
        Some(SessionError::SessionPathUnsafe)
    );
}

#[test]
fn listing_is_newest_first_with_ties_broken_by_descending_id() {
    let fixture = Fixture::new();
    fixture.seed("older", "/workspace", 1, 100);
    fixture.seed("newer", "/workspace", 1, 300);
    fixture.seed("tie-a", "/workspace", 1, 200);
    fixture.seed("tie-b", "/workspace", 1, 200);
    let page = fixture
        .reader("/workspace")
        .resumable_page(ListScope::AllWorkspaces, None, None, 10)
        .unwrap();
    assert_eq!(ids(&page), ["newer", "tie-b", "tie-a", "older"]);
    assert_eq!(page.summaries[0].updated_at_ms, 300_000);
    assert_eq!(page.summaries[0].history_len, 1);
    assert_eq!(page.summaries[0].conversation_language, "en");
}

#[test]
fn resumable_pages_filter_before_paging_and_preserve_continuation_order() {
    let fixture = Fixture::new();
    fixture.seed("a1", "/a", 1, 10);
    fixture.seed("b1", "/b", 1, 20);
    fixture.seed("a2", "/a", 2, 30);
    fixture.seed("empty", "/a", 0, 40);
    fixture.seed("a3", "/a", 1, 50);
    let store = fixture.reader("/a");
    let first = store
        .resumable_page(ListScope::CurrentWorkspace, Some("a3"), None, 1)
        .unwrap();
    assert_eq!(ids(&first), ["a2"]);
    assert!(first.has_more);
    let after = ResumeContinuation {
        updated_at_ms: first.summaries[0].updated_at_ms,
        id: first.summaries[0].id.clone(),
    };
    let second = store
        .resumable_page(ListScope::CurrentWorkspace, Some("a3"), Some(&after), 1)
        .unwrap();
    assert_eq!(ids(&second), ["a1"]);
    assert!(!second.has_more);
    let everywhere = store
        .resumable_page(ListScope::AllWorkspaces, None, None, 10)
        .unwrap();
    assert_eq!(ids(&everywhere), ["a3", "a2", "b1", "a1"]);
}

#[test]
fn checkpoints_make_a_session_resumable_and_listing_counts_turns() {
    let fixture = Fixture::new();
    fixture.seed("compacted", "/w", 2, 10);
    let store = fixture.store("/w");
    let mut session = store.resume("compacted").unwrap();
    session
        .append(
            4,
            &[ConversationEvent::ContextCheckpoint(
                ContextCheckpointEvent {
                    covers_through_seq: 6,
                    summary: "all of it".to_owned(),
                },
            )],
        )
        .unwrap();
    drop(session);
    let page = store
        .resumable_page(ListScope::CurrentWorkspace, None, None, 10)
        .unwrap();
    assert_eq!(page.summaries.len(), 1);
    assert_eq!(page.summaries[0].history_len, 2);
    assert!(page.summaries[0].has_checkpoint);
}

#[test]
fn listing_skips_entries_that_are_not_sessions() {
    let fixture = Fixture::new();
    fixture.seed("good", "/w", 1, 10);
    let sessions = fixture.data().join("sessions");
    fs::write(sessions.join(".resume-catalog"), "not a session").unwrap();
    fs::create_dir(sessions.join("creating+0011")).unwrap();
    fs::create_dir(sessions.join("v2")).unwrap();
    fs::create_dir(sessions.join("broken")).unwrap();
    fs::write(
        sessions.join("broken/session.json"),
        "{\"schema_version\":1,",
    )
    .unwrap();
    symlink(sessions.join("good"), sessions.join("alias")).unwrap();
    let page = fixture
        .reader("/w")
        .resumable_page(ListScope::AllWorkspaces, None, None, 10)
        .unwrap();
    assert_eq!(ids(&page), ["good"]);
}

#[test]
fn resume_latest_opens_the_newest_session_of_this_workspace_even_when_empty() {
    let fixture = Fixture::new();
    let store = fixture.store("/w");
    assert_eq!(
        store.resume_latest().err(),
        Some(SessionError::NoSavedSessions)
    );
    fixture.seed("older", "/w", 1, 10);
    fixture.seed("empty", "/w", 0, 20);
    fixture.seed("elsewhere", "/other", 1, 30);
    let latest = store.resume_latest().unwrap();
    assert_eq!(latest.id(), "empty");
    drop(latest);
    fs::remove_file(fixture.session_dir("empty").join("session.json")).unwrap();
    let mut latest = store.resume_latest().unwrap();
    assert_eq!(latest.id(), "older");
    assert_eq!(latest.take_history().turns.len(), 1);
}

#[test]
fn workspace_latest_distinguishes_corrupt_only_storage_from_no_saved_sessions() {
    let fixture = Fixture::new();
    let store = fixture.store("/w");
    fs::create_dir(fixture.session_dir("broken-only")).unwrap();
    fs::write(
        fixture.session_dir("broken-only").join("session.json"),
        "{\"schema_version\":1,",
    )
    .unwrap();
    assert_eq!(
        store.resume_latest().err(),
        Some(SessionError::NoReadableSessions)
    );
}

#[test]
fn latest_resume_reports_a_busy_session_instead_of_skipping_it() {
    let fixture = Fixture::new();
    fixture.seed("older", "/w", 1, 10);
    fixture.seed("newer", "/w", 1, 20);
    let mut store = fixture.store("/w");
    let held = store.resume("newer").unwrap();
    store.lock_deadline = Duration::ZERO;
    assert_eq!(store.resume_latest().err(), Some(SessionError::SessionBusy));
    drop(held);
}

#[test]
fn a_fifo_never_blocks_listing_or_latest_resume() {
    for file in ["events.jsonl", "session.json"] {
        let fixture = Fixture::new();
        fixture.seed("older", "/w", 1, 10);
        fixture.seed("fifo", "/w", 1, 20);
        let path = fixture.session_dir("fifo").join(file);
        fs::remove_file(&path).unwrap();
        assert!(
            Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let store = fixture.store("/w");
        let page = store
            .resumable_page(ListScope::AllWorkspaces, None, None, 10)
            .unwrap();
        assert_eq!(ids(&page), ["older"], "{file}");
        assert_eq!(store.resume_latest().unwrap().id(), "older", "{file}");
    }
}

#[test]
fn explicit_resume_rebinds_the_workspace_and_keeps_the_origin() {
    let fixture = Fixture::new();
    let id = fixture
        .store("/workspace-a")
        .start(preferences())
        .unwrap()
        .id()
        .to_owned();
    let target = fixture.store("/workspace-b");
    let resumed = target.resume(&id).unwrap();
    assert_eq!(resumed.metadata().workspace_root, "/workspace-b");
    assert_eq!(resumed.metadata().origin_workspace_root, "/workspace-a");
    drop(resumed);
    let reopened = target.load(&id).unwrap();
    assert_eq!(reopened.metadata.workspace_root, "/workspace-b");
    assert_eq!(reopened.metadata.origin_workspace_root, "/workspace-a");
    assert_eq!(
        fixture.store("/workspace-a").resume_latest().err(),
        Some(SessionError::NoSavedSessions)
    );
    assert_eq!(target.resume_latest().unwrap().id(), id);
}

#[test]
fn remembered_session_ids_are_bounded_and_unambiguous() {
    assert_eq!(parse_remembered_session_id(b"session-1\n"), Ok("session-1"));
    for bytes in [
        &b""[..],
        b"\n",
        b"../other\n",
        b"session-1",
        b"one\ntwo\n",
        b"one\r\n",
        b" one\n",
        b"v2\n",
        b"\xffx\n",
    ] {
        assert_eq!(
            parse_remembered_session_id(bytes),
            Err(SessionError::InvalidRememberedSession),
            "{bytes:?}"
        );
    }
    let mut bytes = [b'a'; 257];
    bytes[255] = b'\n';
    assert_eq!(
        parse_remembered_session_id(&bytes[..256]).unwrap().len(),
        255
    );
    assert_eq!(
        parse_remembered_session_id(&bytes),
        Err(SessionError::InvalidRememberedSession)
    );
}

#[test]
fn remembered_session_selection_is_private_per_workspace_and_read_only_lookup_creates_nothing() {
    let fixture = Fixture::new();
    let store = fixture.store("/workspace");
    assert_eq!(store.remembered_session_id(), Ok(None));
    assert!(!fixture.data().join("continue").exists());
    store.remember_session_id("first").unwrap();
    store.remember_session_id("second").unwrap();
    let reader = fixture.reader("/workspace/");
    assert_eq!(
        reader.remembered_session_id(),
        Ok(Some("second".to_owned()))
    );
    assert_eq!(
        reader.remember_session_id("third"),
        Err(SessionError::SessionStoreReadOnly)
    );
    assert_eq!(
        store.remember_session_id("../x"),
        Err(SessionError::InvalidSessionId)
    );
    assert_eq!(
        fixture.reader("/other-workspace").remembered_session_id(),
        Ok(None)
    );

    let directory = fixture.data().join("continue");
    assert_eq!(mode(&directory), 0o700);
    let name = remembered_file_name("/workspace");
    assert_eq!(name.len(), 64);
    let file = directory.join(&name);
    assert_eq!(mode(&file), 0o600);
    assert_eq!(fs::read(&file).unwrap(), b"second\n");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o400)).unwrap();
    assert_eq!(
        store.remember_session_id("third"),
        Err(SessionError::Storage(
            ofx_config::DurableError::AccessDenied
        ))
    );
    assert_eq!(
        reader.remembered_session_id(),
        Err(SessionError::InvalidRememberedSession)
    );
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        reader.remembered_session_id(),
        Ok(Some("second".to_owned()))
    );

    fs::write(&file, "two\nlines\n").unwrap();
    assert_eq!(
        reader.remembered_session_id(),
        Err(SessionError::InvalidRememberedSession)
    );
    fs::remove_file(&file).unwrap();
    symlink("elsewhere", &file).unwrap();
    assert_eq!(
        reader.remembered_session_id(),
        Err(SessionError::InvalidRememberedSession)
    );
    fs::remove_file(&file).unwrap();

    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        reader.remembered_session_id(),
        Err(SessionError::SessionPathUnsafe)
    );
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(fixture.data(), fs::Permissions::from_mode(0o750)).unwrap();
    assert_eq!(
        reader.remembered_session_id(),
        Err(SessionError::SessionPathUnsafe)
    );
    assert_eq!(
        store.remember_session_id("third"),
        Err(SessionError::SessionPathUnsafe)
    );
}
