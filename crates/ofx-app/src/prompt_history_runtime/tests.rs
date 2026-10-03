use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use super::*;

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

    fn history(&self) -> PathBuf {
        self.data().join("history.jsonl")
    }

    fn runtime(&self, workspace: &str) -> PromptHistoryRuntime {
        PromptHistoryRuntime::initialize(Some(&self.data()), Path::new(workspace))
    }

    fn saved(&self) -> String {
        fs::read_to_string(self.history()).unwrap_or_default()
    }
}

fn body(notice: Option<Notice>) -> Option<String> {
    notice.map(|notice| {
        assert_eq!(notice.tone, NoticeTone::Warning);
        assert_eq!(notice.topic, "history");
        notice.body
    })
}

#[test]
fn accepted_prompts_reach_later_sessions_of_the_same_workspace() {
    let fixture = Fixture::new();
    let mut first = fixture.runtime("/work/a");
    assert_eq!(first.load_recent(), Ok(Vec::new()));
    first.record_accepted(1, "one").unwrap();
    first.record_accepted(2, "/help").unwrap();
    fixture
        .runtime("/work/b")
        .record_accepted(3, "elsewhere")
        .unwrap();
    assert_eq!(
        fixture.runtime("/work/a").load_recent(),
        Ok(vec!["one".to_owned(), "/help".to_owned()])
    );
    let (_, notice) = fixture.runtime("/work/a").into_shell_history(true);
    assert_eq!(body(notice), None);
}

#[test]
fn a_missing_profile_directory_reports_history_as_unavailable() {
    let (_, notice) =
        PromptHistoryRuntime::initialize(None, Path::new("/work")).into_shell_history(true);
    assert_eq!(
        body(notice).as_deref(),
        Some("durable prompt history unavailable")
    );
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.path().join("elsewhere")).unwrap();
    fs::create_dir_all(fixture.root.path().join("data")).unwrap();
    symlink(fixture.root.path().join("elsewhere"), fixture.data()).unwrap();
    let mut runtime = fixture.runtime("/work");
    assert_eq!(runtime.record_accepted(1, "kept out"), Ok(()));
    assert!(
        fs::read_dir(fixture.root.path().join("elsewhere"))
            .unwrap()
            .next()
            .is_none()
    );
    let (_, notice) = runtime.into_shell_history(false);
    assert_eq!(
        body(notice).as_deref(),
        Some("durable prompt history unavailable")
    );
}

#[test]
fn a_failed_load_warns_and_stops_saving_for_the_session() {
    let fixture = Fixture::new();
    fixture.runtime("/work").record_accepted(1, "kept").unwrap();
    fs::set_permissions(fixture.history(), fs::Permissions::from_mode(0o644)).unwrap();
    let mut runtime = fixture.runtime("/work");
    let notice = runtime.load_recent().unwrap_err();
    assert_eq!(
        notice.body,
        "failed to load durable prompt history (PrivateStatePermissionsUnsupported)"
    );
    runtime.record_accepted(2, "dropped").unwrap();
    assert!(!fixture.saved().contains("dropped"));
    let (_, notice) = fixture.runtime("/work").into_shell_history(true);
    assert_eq!(
        body(notice).as_deref(),
        Some("failed to load durable prompt history (PrivateStatePermissionsUnsupported)")
    );
}

#[test]
fn disabled_history_reads_nothing() {
    let fixture = Fixture::new();
    fixture.runtime("/work").record_accepted(1, "kept").unwrap();
    fs::set_permissions(fixture.history(), fs::Permissions::from_mode(0o644)).unwrap();
    let (_, notice) = fixture.runtime("/work").into_shell_history(false);
    assert_eq!(body(notice), None);
}

#[test]
fn a_workspace_root_that_is_not_utf8_fails_the_load() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let fixture = Fixture::new();
    let mut runtime = PromptHistoryRuntime::initialize(
        Some(&fixture.data()),
        Path::new(OsStr::from_bytes(b"/work/\xff")),
    );
    assert_eq!(
        runtime.load_recent().unwrap_err().body,
        "failed to load durable prompt history (InvalidDurableField)"
    );
}

#[test]
fn appends_report_their_failure_by_name() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime("relative");
    assert_eq!(
        runtime
            .record_accepted(1, "x")
            .map_err(|error| error.to_string()),
        Err("InvalidDurableField".to_owned())
    );
}
