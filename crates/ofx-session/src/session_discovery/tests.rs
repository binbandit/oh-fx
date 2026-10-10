use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use ofx_config::ProviderId;
use ofx_contract::ReasoningEffort;

use super::*;
use crate::session_codec::{SavedProvider, SessionMetadata, SessionPreferences};
use crate::session_event::{AssistantEvent, TurnCompletedEvent, UserEvent};
use crate::session_log::start_session;

struct Store {
    root: tempfile::TempDir,
    sessions: PrivateDir,
}

impl Store {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let sessions = PrivateDir::open_or_create(&root.path().join("sessions")).unwrap();
        Self { root, sessions }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.root.path().join("sessions").join(id)
    }

    fn seed(&self, id: &str, subagent_child: bool) -> PathBuf {
        let mut session = start_session(&self.sessions, metadata(id, subagent_child)).unwrap();
        session
            .append(
                3,
                &[
                    ConversationEvent::User(UserEvent::new("hello")),
                    ConversationEvent::Assistant(AssistantEvent {
                        text: "hi".to_owned(),
                        provider_replay: None,
                        standalone_response: false,
                    }),
                    ConversationEvent::TurnCompleted(TurnCompletedEvent::default()),
                ],
            )
            .unwrap();
        self.path(id)
    }

    fn inspect(&self, limit: usize) -> DoctorInspection {
        inspect_for_doctor(&self.sessions, limit).unwrap()
    }

    fn issues(&self) -> Vec<(String, DoctorIssueKind)> {
        let mut issues: Vec<_> = self
            .inspect(64)
            .diagnostics
            .into_iter()
            .map(|diagnostic| (diagnostic.session_id, diagnostic.kind))
            .collect();
        issues.sort_by(|a, b| a.0.cmp(&b.0));
        issues
    }
}

fn metadata(id: &str, subagent_child: bool) -> SessionMetadata {
    SessionMetadata {
        id: id.to_owned(),
        origin_workspace_root: "/workspace".to_owned(),
        workspace_root: "/workspace".to_owned(),
        created_at_ms: 1,
        updated_at_ms: 2,
        conversation_language: "en".to_owned(),
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

fn private_dir(path: &Path) -> &Path {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn private_file(path: &Path, bytes: &str) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn readable_sessions_and_their_private_side_folders_report_nothing() {
    let store = Store::new();
    let kept = store.seed("kept", false);
    store.seed("child", true);
    for parent in ["logs", "artifacts"] {
        private_dir(&kept.join(parent));
    }
    for route in [
        "logs/commands",
        "artifacts/browser",
        "tool-results",
        "subagent",
    ] {
        let dir = kept.join(route);
        private_dir(&dir);
        private_file(&dir.join("entry-1.txt"), "saved");
    }
    private_dir(&kept.join("logs/elsewhere/inner"));
    let inspection = store.inspect(64);
    assert_eq!(inspection.inspected_count, 2);
    assert!(!inspection.truncated);
    assert_eq!(inspection.diagnostics, []);
}

#[test]
fn sessions_that_do_not_load_are_invalid() {
    let store = Store::new();
    fs::write(store.seed("broken-log", false).join("events.jsonl"), "{\n").unwrap();
    private_file(
        &store.seed("broken-recovery", false).join("recovery.json"),
        "{",
    );
    private_file(
        &store.seed("broken-manifest", false).join("session.json"),
        "{",
    );
    let missing_log = store.seed("missing-log", false);
    fs::remove_file(missing_log.join("events.jsonl")).unwrap();
    private_dir(&store.path("empty"));
    assert_eq!(
        store.issues(),
        [
            (
                "broken-log".to_owned(),
                DoctorIssueKind::CanonicalStateInvalid
            ),
            (
                "broken-manifest".to_owned(),
                DoctorIssueKind::CanonicalStateInvalid
            ),
            (
                "broken-recovery".to_owned(),
                DoctorIssueKind::CanonicalStateInvalid
            ),
            ("empty".to_owned(), DoctorIssueKind::CanonicalStateInvalid),
            (
                "missing-log".to_owned(),
                DoctorIssueKind::CanonicalStateInvalid
            ),
        ]
    );
}

#[test]
fn a_pending_authority_transition_is_reported_before_the_folder_is_read() {
    let store = Store::new();
    for (id, name) in [
        ("authority", "authority.pending.json"),
        ("commit", "commit.pending.json"),
    ] {
        private_file(&private_dir(&store.path(id)).join(name), "{}");
    }
    assert_eq!(
        store.issues(),
        [
            (
                "authority".to_owned(),
                DoctorIssueKind::AuthorityTransitionPending
            ),
            (
                "commit".to_owned(),
                DoctorIssueKind::AuthorityTransitionPending
            ),
        ]
    );
}

#[test]
fn side_folders_that_are_not_private_regular_files_are_unsafe() {
    let store = Store::new();
    let open_folder = store.seed("open-folder", false).join("tool-results");
    fs::create_dir_all(&open_folder).unwrap();
    fs::set_permissions(&open_folder, fs::Permissions::from_mode(0o755)).unwrap();
    private_file(
        &private_dir(&store.seed("open-file", false).join("subagent")).join("owner.json"),
        "{}",
    );
    fs::set_permissions(
        store.path("open-file").join("subagent/owner.json"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    private_dir(&store.seed("nested", false).join("logs/commands/inner"));
    private_file(
        &private_dir(&store.seed("named", false).join("tool-results")).join("bad name"),
        "x",
    );
    let target = store.root.path().join("elsewhere");
    private_dir(&target);
    symlink(&target, store.seed("linked", false).join("tool-results")).unwrap();
    let linked_file = private_dir(&store.seed("hard", false).join("tool-results")).join("a");
    private_file(&linked_file, "x");
    fs::hard_link(&linked_file, linked_file.with_file_name("b")).unwrap();
    assert_eq!(
        store.issues(),
        [
            ("hard".to_owned(), DoctorIssueKind::UnsafePath),
            ("linked".to_owned(), DoctorIssueKind::UnsafePath),
            ("named".to_owned(), DoctorIssueKind::UnsafePath),
            ("nested".to_owned(), DoctorIssueKind::UnsafePath),
            ("open-file".to_owned(), DoctorIssueKind::UnsafePath),
            ("open-folder".to_owned(), DoctorIssueKind::UnsafePath),
        ]
    );
}

#[test]
fn only_session_folders_count_and_inspection_stops_at_its_limit() {
    let store = Store::new();
    for id in ["one", "two", "three"] {
        store.seed(id, false);
    }
    private_dir(&store.path("latest"));
    private_dir(&store.path("creating+0011"));
    fs::write(store.path("loose-file"), "").unwrap();
    symlink(store.path("one"), store.path("linked")).unwrap();
    let everything = store.inspect(64);
    assert_eq!(everything.inspected_count, 3);
    assert!(!everything.truncated);
    let bounded = store.inspect(2);
    assert_eq!(bounded.inspected_count, 2);
    assert!(bounded.truncated);
    let exact = store.inspect(3);
    assert_eq!(exact.inspected_count, 3);
    assert!(!exact.truncated);
}

#[test]
fn issue_kinds_keep_upstreams_names() {
    assert_eq!(
        [
            DoctorIssueKind::AuthorityTransitionPending,
            DoctorIssueKind::CanonicalStateInvalid,
            DoctorIssueKind::UnsafePath,
        ]
        .map(DoctorIssueKind::name),
        [
            "authority_transition_pending",
            "canonical_state_invalid",
            "unsafe_path"
        ]
    );
}
