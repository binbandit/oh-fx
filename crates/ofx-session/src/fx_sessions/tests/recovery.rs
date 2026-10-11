use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use ofx_config::PrivateDir;

use super::{Home, Saved, shell_turn, snapshot};
use crate::session_error::SessionError;
use crate::session_store::SessionStore;
use crate::session_store_types::{SessionRecovery, SessionRecoveryStatus};

const WORKSPACE: &str = "/work";
const SAVED_RESULT: &str = "result-shell-0011223344556677-8899aabbccddeeff.txt";
const RESULT: &str = "result-shell-0011223344556677-a9ab0f8705989963.txt";
const LEGACY_REPLAY: &str = "fx-command-replay-legacy.bin";
const REPLAY: &str = "fx-command-replay-aae1a4c602fd36c8.bin";

fn saved<'a>(id: &'a str, prompts: &'a [&'a str]) -> Saved<'a> {
    Saved {
        id,
        workspace: WORKSPACE,
        title: Some("Saved title"),
        prompts,
        modified_s: 100,
    }
}

fn store(home: &Home) -> SessionStore {
    home.store(WORKSPACE)
        .with_fx_home(home.path().to_path_buf())
}

fn append(path: &Path, bytes: &[u8]) {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

fn private_file(path: &Path, bytes: &[u8]) {
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent).unwrap();
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn private_dirs(root: &Path, relative: &str) {
    let mut path = root.to_path_buf();
    for component in relative.split('/') {
        path.push(component);
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn recovered(source: &str, copy: &SessionRecovery, history_len: usize) -> SessionRecovery {
    SessionRecovery {
        source_session_id: source.to_owned(),
        recovered_session_id: copy.recovered_session_id.clone(),
        history_len,
        usage_incomplete: false,
        status: SessionRecoveryStatus::Recovered,
    }
}

#[test]
fn a_damaged_session_is_copied_up_to_its_last_good_turn_and_left_as_it_was() {
    let home = Home::new();
    let store = store(&home);
    saved("damaged", &["one", "two"]).write(&home.own_sessions());
    let log = home.own_sessions().join("damaged/events.jsonl");
    append(&log, b"invalid\n{\"torn");
    let before = fs::read(&log).unwrap();

    let copy = store.recover("damaged").unwrap();
    assert_eq!(copy, recovered("damaged", &copy, 2));
    assert_ne!(copy.recovered_session_id, "damaged");
    let restored = store.load(&copy.recovered_session_id).unwrap();
    assert_eq!(restored.history.turns.len(), 2);
    assert_eq!(restored.metadata.title.as_deref(), Some("Saved title"));
    assert_eq!(restored.metadata.workspace_root, WORKSPACE);
    let copy_dir = home.own_sessions().join(&copy.recovered_session_id);
    assert!(copy_dir.join("usage-v2.json").exists());
    assert_eq!(
        fs::read(copy_dir.join("events.jsonl")).unwrap(),
        before[..before.len() - b"invalid\n{\"torn".len()]
    );
    assert_eq!(fs::read(&log).unwrap(), before);
    assert_eq!(
        store.recover(&copy.recovered_session_id),
        Err(SessionError::SessionRecoveryNotNeeded)
    );
}

#[test]
fn a_healthy_session_needs_no_recovery_unless_it_does_not_load() {
    let home = Home::new();
    let store = store(&home);
    saved("healthy", &["one"]).write(&home.own_sessions());
    assert_eq!(
        store.recover("healthy"),
        Err(SessionError::SessionRecoveryNotNeeded)
    );
    private_file(&home.own_sessions().join("healthy/recovery.json"), b"{");
    assert_eq!(
        store.recover("healthy"),
        Err(SessionError::InvalidRecoveryCheckpoint)
    );
}

#[test]
fn an_empty_usage_record_is_recovered_with_its_usage_marked_incomplete() {
    let home = Home::new();
    let store = store(&home);
    saved("usage", &["one"]).write(&home.own_sessions());
    private_file(&home.own_sessions().join("usage/usage-v2.json"), b"");
    let copy = store.recover("usage").unwrap();
    assert!(copy.usage_incomplete);
    assert_eq!(copy.status, SessionRecoveryStatus::Recovered);
    assert_eq!(copy.history_len, 1);
}

fn shell_session(home: &Home, id: &str, replay: &str, output: &[u8]) {
    let session = saved(id, &[]);
    let mut events = shell_turn(&format!("\"{replay}\""), "4").replace(SAVED_RESULT, RESULT);
    events.push_str("{\"torn");
    session.write_files(&home.own_sessions(), &session.manifest(), &events);
    let dir = home.own_sessions().join(id);
    private_dirs(&dir, "tool-results");
    private_file(&dir.join("tool-results").join(RESULT), output);
    private_dirs(&dir, "logs/commands");
    private_file(&dir.join("logs/commands").join(replay), b"fx!!");
}

#[test]
fn the_artifacts_kept_turns_name_are_copied_and_unauthenticated_replays_are_reported() {
    let home = Home::new();
    let store = store(&home);
    shell_session(&home, "replayed", LEGACY_REPLAY, b"a.t");

    let copy = store.recover("replayed").unwrap();
    assert_eq!(
        copy.status,
        SessionRecoveryStatus::RecoveredWithUnverifiedArtifacts
    );
    let copied = home.own_sessions().join(&copy.recovered_session_id);
    assert_eq!(
        fs::read(copied.join("tool-results").join(RESULT)).unwrap(),
        b"a.t"
    );
    assert_eq!(
        fs::read(copied.join("logs/commands").join(LEGACY_REPLAY)).unwrap(),
        b"fx!!"
    );

    shell_session(&home, "authenticated", REPLAY, b"a.t");
    assert_eq!(
        store.recover("authenticated").unwrap().status,
        SessionRecoveryStatus::Recovered
    );
    let forged = format!("fx-command-replay-{}.bin", "0".repeat(64));
    shell_session(&home, "forged-replay", &forged, b"a.t");
    shell_session(&home, "changed-output", REPLAY, b"a.x");
    for id in ["forged-replay", "changed-output"] {
        assert_eq!(
            store.recover(id),
            Err(SessionError::SessionRecoveryBoundaryInvalid),
            "{id}"
        );
    }

    let plain = saved("missing-output", &[]);
    let mut events = shell_turn("null", "null").replace(SAVED_RESULT, RESULT);
    events.push_str("{\"torn");
    plain.write_files(&home.own_sessions(), &plain.manifest(), &events);
    assert_eq!(
        store.recover("missing-output"),
        Err(SessionError::SessionRecoveryBoundaryInvalid)
    );
    private_dirs(&home.own_sessions().join("missing-output"), "tool-results");
    private_file(
        &home
            .own_sessions()
            .join("missing-output/tool-results")
            .join(RESULT),
        b"too long",
    );
    assert_eq!(
        store.recover("missing-output"),
        Err(SessionError::SessionRecoveryBoundaryInvalid)
    );
    assert_eq!(
        fs::read_dir(home.own_sessions())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("creating+")
            })
            .count(),
        0
    );
}

#[test]
fn a_damaged_session_fx_saved_is_recovered_into_oh_fx_and_fx_is_left_alone() {
    let home = Home::new();
    let store = store(&home);
    saved("fx-damaged", &["one"]).write(&home.fx_sessions());
    append(
        &home.fx_sessions().join("fx-damaged/events.jsonl"),
        b"invalid\n",
    );
    let before = snapshot(&home.fx_profile());
    let copy = store.recover("fx-damaged").unwrap();
    assert_eq!(copy, recovered("fx-damaged", &copy, 1));
    assert_eq!(
        store
            .load(&copy.recovered_session_id)
            .unwrap()
            .history
            .turns
            .len(),
        1
    );
    assert!(!home.own_sessions().join("fx-damaged").exists());
    assert_eq!(snapshot(&home.fx_profile()), before);
}

#[test]
fn a_session_that_cannot_be_recovered_names_why() {
    let home = Home::new();
    let store = store(&home);
    let future = home.own_sessions().join("future");
    private_dirs(&home.own_sessions(), "future");
    private_file(&future.join("session.json"), b"{\"schema_version\":99}");
    let child = saved("child", &["one"]);
    child.write_with(
        &home.own_sessions(),
        &child
            .manifest()
            .replace("\"subagent_child\":false", "\"subagent_child\":true"),
    );
    append(
        &home.own_sessions().join("child/events.jsonl"),
        b"invalid\n",
    );
    for (id, error) in [
        ("../x", SessionError::InvalidSessionId),
        ("missing", SessionError::SessionNotFound),
        ("future", SessionError::SessionRecoveryRequiresCurrentSchema),
        ("child", SessionError::SessionNotFound),
    ] {
        assert_eq!(store.recover(id), Err(error), "{id}");
    }

    saved("busy", &["one"]).write(&home.own_sessions());
    append(&home.own_sessions().join("busy/events.jsonl"), b"invalid\n");
    let sessions = PrivateDir::open_existing(&home.own_sessions())
        .unwrap()
        .unwrap();
    let _held = sessions
        .open_child("busy")
        .unwrap()
        .unwrap()
        .try_lock("session.lock")
        .unwrap()
        .unwrap();
    assert_eq!(store.recover("busy"), Err(SessionError::SessionBusy));
}
