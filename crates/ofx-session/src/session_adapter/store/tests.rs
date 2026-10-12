use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;
use crate::session_manager::test_support::framed;
use crate::session_store::ListScope;

const FIXTURE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/sessions_v2/listing/index.jsonl"
));
const WORKSPACE: &str = "/private/tmp/fxv2/listing/workspace";
const A: &str = "nJI52ukeF2bU";
const B: &str = "U9K-4ZzltsUh";
const C: &str = "pgdu6JEc1UOO";
const D: &str = "hDoX4WdPFWDK";

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn data(&self) -> PathBuf {
        self.root.path().join("data")
    }

    fn fx_root(&self) -> PathBuf {
        self.home().join(".fx/sessions/v2")
    }

    fn own_root(&self) -> PathBuf {
        self.data().join("sessions/v2")
    }

    fn write_root(root: &Path, index: &[u8], sessions: &[&str]) {
        fs::create_dir_all(root).unwrap();
        let mut folder = root.to_path_buf();
        for _ in 0..3 {
            fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
            folder.pop();
        }
        fs::write(root.join("index.jsonl"), index).unwrap();
        for id in sessions {
            fs::create_dir(root.join(id)).unwrap();
        }
    }

    fn open(&self) -> SessionsV2 {
        SessionsV2::open_read_only(&self.data(), &self.home(), WORKSPACE).unwrap()
    }

    fn listed(&self, scope: ListScope) -> Vec<(String, SessionSource)> {
        self.open()
            .catalog()
            .unwrap()
            .listed_page(scope, None, 50)
            .summaries
            .into_iter()
            .map(|summary| (summary.id, summary.source))
            .collect()
    }
}

fn put(id: &str, title: &str, updated_ms: u64) -> Vec<u8> {
    framed(&format!(
        "{{\"op\":\"put\",\"id\":\"{id}\",\"role\":\"root\",\"host\":\"ask\",\"workspace\":\"{WORKSPACE}\",\"title\":{title},\"created\":1,\"updated\":{updated_ms},\"turns\":0,\"opened\":[0,0,0,0,0]"
    ))
}

fn fx(id: &str) -> (String, SessionSource) {
    (id.to_owned(), SessionSource::Fx)
}

#[test]
fn the_sessions_fx_saved_on_v2_list_newest_first_and_marked_fx() {
    let fixture = Fixture::new();
    Fixture::write_root(&fixture.fx_root(), FIXTURE, &[A, B, C, D]);
    assert_eq!(
        fixture.listed(ListScope::AllWorkspaces),
        [fx(D), fx(C), fx(B), fx(A)]
    );
    assert_eq!(
        fixture.listed(ListScope::CurrentWorkspace),
        [fx(D), fx(C), fx(A)]
    );
    let catalog = fixture.open().catalog().unwrap();
    let a = catalog
        .summaries()
        .iter()
        .find(|summary| summary.id == A)
        .unwrap();
    assert_eq!(a.title.as_deref(), Some("List question A."));
    assert_eq!(a.history_len, 2);
    assert_eq!(a.conversation_language, "und-Latn");
    assert_eq!(a.workspace_root, WORKSPACE);
    assert_eq!(a.origin_workspace_root, WORKSPACE);
    assert_eq!(
        (a.created_at_ms, a.updated_at_ms),
        (1_791_772_881_566, 1_791_772_881_704)
    );
    assert!(!fixture.own_root().exists());
}

#[test]
fn an_own_session_hides_the_fx_session_of_the_same_id() {
    let fixture = Fixture::new();
    Fixture::write_root(&fixture.fx_root(), FIXTURE, &[A, B, C, D]);
    Fixture::write_root(&fixture.own_root(), &put(A, "\"Copied.\"", 5), &[A]);
    let listed = fixture.listed(ListScope::AllWorkspaces);
    assert_eq!(
        listed,
        [fx(D), fx(C), fx(B), (A.to_owned(), SessionSource::OhFx)]
    );
    let catalog = fixture.open().catalog().unwrap();
    assert_eq!(catalog.summaries()[3].title.as_deref(), Some("Copied."));
    assert_eq!(catalog.summaries()[3].history_len, 1);
}

#[test]
fn an_unreadable_fx_entry_is_hidden_and_an_unreadable_own_entry_fails_the_listing() {
    let fixture = Fixture::new();
    let mut index = put("good", "\"Good.\"", 3);
    index.extend(put("bad", "5", 4));
    Fixture::write_root(&fixture.fx_root(), &index, &["good", "bad"]);
    assert_eq!(fixture.listed(ListScope::AllWorkspaces), [fx("good")]);
    Fixture::write_root(
        &fixture.own_root(),
        &put("mine", "\"Mine\\u0000\"", 5),
        &["mine"],
    );
    assert!(fixture.open().catalog().is_ok());
    fs::write(
        fixture.own_root().join("index.jsonl"),
        framed(&format!(
            "{{\"op\":\"put\",\"id\":\"mine\",\"role\":\"root\",\"host\":\"ask\",\"workspace\":\"{WORKSPACE}\",\"language\":\"\",\"created\":1,\"updated\":5,\"turns\":0,\"opened\":[0,0,0,0,0]"
        )),
    )
    .unwrap();
    assert!(matches!(
        fixture.open().catalog(),
        Err(SessionError::InvalidSessionFormat)
    ));
}

#[test]
fn an_fx_root_that_is_not_private_is_not_read() {
    let fixture = Fixture::new();
    Fixture::write_root(&fixture.fx_root(), FIXTURE, &[A, B, C, D]);
    fs::set_permissions(fixture.fx_root(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(fixture.listed(ListScope::AllWorkspaces).is_empty());
}

#[test]
fn no_roots_list_nothing_and_create_nothing() {
    let fixture = Fixture::new();
    assert!(fixture.listed(ListScope::AllWorkspaces).is_empty());
    assert!(!fixture.data().exists());
    assert!(!fixture.home().exists());
}

const TRACE_CHILD: &str = "OH_FX_SESSIONS_V2_TRACE_CHILD";
const TRACE_TEST: &str =
    "session_adapter::store::tests::listing_failures_write_upstream_trace_lines";

fn fail_listings(root: &Path) {
    let fixture = Fixture {
        root: tempfile::tempdir_in(root).unwrap(),
    };
    Fixture::write_root(&fixture.own_root(), &put("mine", "\"Mine.\"", 5), &["mine"]);
    let index = fixture.own_root().join("index.jsonl");
    fs::set_permissions(&index, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(matches!(
        fixture.open().catalog(),
        Err(SessionError::SessionStoreUnavailable)
    ));
    fs::set_permissions(&index, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &index,
        framed(&format!(
            "{{\"op\":\"put\",\"id\":\"mine\",\"role\":\"root\",\"host\":\"ask\",\"workspace\":\"{WORKSPACE}\",\"language\":\" en\",\"created\":1,\"updated\":5,\"turns\":0,\"opened\":[0,0,0,0,0]"
        )),
    )
    .unwrap();
    assert!(matches!(
        fixture.open().catalog(),
        Err(SessionError::InvalidSessionFormat)
    ));
}

#[test]
fn listing_failures_write_upstream_trace_lines() {
    if let Some(root) = std::env::var_os(TRACE_CHILD) {
        ofx_trace::configure_from_env(Path::new(&root), None);
        fail_listings(Path::new(&root));
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let probe = root.path().join("probe");
    fs::write(&probe, "").unwrap();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&probe).is_ok() {
        return;
    }
    let log = root.path().join("trace.log");
    let status = process::Command::new(std::env::current_exe().unwrap())
        .args([TRACE_TEST, "--exact", "--test-threads=1"])
        .env_clear()
        .env(TRACE_CHILD, root.path())
        .env("OH_FX_TRACE_LOG", &log)
        .stdout(process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "{status:?}");
    let written = fs::read_to_string(&log).unwrap();
    let pid = format!(" pid={}\n", trace_pid(&written));
    for line in [
        format!(" [wiring] action=StoreOpen session=-{pid}"),
        " [session] event=sessions_v2_command_failed kind=store err=AccessDenied\n".to_owned(),
        " [session] event=sessions_v2_command_failed kind=record err=InvalidDurableField\n"
            .to_owned(),
        format!(" [wiring] action=StoreClose session=-{pid}"),
    ] {
        assert!(written.contains(&line), "{line}{written}");
    }
}

fn trace_pid(trace: &str) -> &str {
    let marker = "action=StoreOpen session=- pid=";
    let start = trace.find(marker).unwrap() + marker.len();
    let end = start + trace[start..].find('\n').unwrap();
    &trace[start..end]
}
