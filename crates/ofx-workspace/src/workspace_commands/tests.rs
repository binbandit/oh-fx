use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};

use serde_json::json;
use tempfile::TempDir;

use super::*;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    primary: PathBuf,
    paths: ProfilePaths,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let primary = root.join("primary");
        fs::create_dir(&primary).unwrap();
        let paths = ProfilePaths {
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
            cache: root.join("cache"),
        };
        Self {
            _temp: temp,
            root,
            primary,
            paths,
        }
    }

    fn directory(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn settings_path(&self) -> PathBuf {
        self.paths.config.join("settings.json")
    }

    fn write_saved(&self, directories: &[String]) {
        DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&self.paths.config)
            .unwrap();
        let key = text(&self.primary);
        fs::write(
            self.settings_path(),
            json!({"workspaces": {key: {"additional_directories": directories}}}).to_string(),
        )
        .unwrap();
    }

    fn durable(&self) -> Vec<String> {
        Settings::load(&self.paths, &self.primary)
            .unwrap()
            .additional_directories()
            .to_vec()
    }

    fn saved_directories(&self, count: usize) -> Vec<String> {
        (0..count)
            .map(|index| text(&self.directory(&format!("saved-{index}"))))
            .collect()
    }
}

fn text(path: &Path) -> String {
    path.to_str().unwrap().to_owned()
}

fn launched(primary: &Path, saved: &[String], directories: &[&Path]) -> WorkspaceAccess {
    let directories: Vec<OsString> = directories
        .iter()
        .map(|path| path.as_os_str().to_owned())
        .collect();
    WorkspaceAccess::new(primary, saved)
        .unwrap()
        .apply_launch(&directories, false)
        .unwrap()
}

fn updated(outcome: Outcome) -> (WorkspaceAccess, Mutation) {
    match outcome {
        Outcome::Updated { access, mutation } => (access, mutation),
        Outcome::Indeterminate(reconciliation) => panic!("indeterminate: {reconciliation:?}"),
    }
}

fn failed(phase: FailurePhase, error: impl Into<CommandError>) -> Failure {
    Failure {
        phase,
        error: error.into(),
    }
}

fn observed<'a>(pairs: &[(&'a str, &'a str)]) -> Vec<Observed<'a>> {
    pairs
        .iter()
        .map(|&(source, identity)| Observed { source, identity })
        .collect()
}

fn patched(entry: Value, change: Patch, observed: Vec<Observed<'_>>) -> (Value, bool) {
    let mut entry = entry;
    let patch = DurablePatch {
        change,
        observed,
        command_line: Vec::new(),
    };
    let changed = apply(&mut entry, &patch).unwrap();
    (entry, changed)
}

#[test]
fn adding_canonicalizes_observed_spellings_and_keeps_unseen_entries() {
    let (entry, changed) = patched(
        json!({"additional_directories": ["/a/./x", "/new/elsewhere"], "model": "m"}),
        Patch::Add("/b".to_owned()),
        observed(&[("/a/./x", "/a/x")]),
    );
    assert!(changed);
    assert_eq!(
        entry,
        json!({"additional_directories": ["/a/x", "/new/elsewhere", "/b"], "model": "m"})
    );
    let (entry, changed) = patched(
        json!({"additional_directories": ["/a/x"]}),
        Patch::Add("/a/x".to_owned()),
        observed(&[("/a/x", "/a/x")]),
    );
    assert!(!changed);
    assert_eq!(entry, json!({"additional_directories": ["/a/x"]}));
    let (entry, changed) = patched(Value::Null, Patch::Add("/b".to_owned()), Vec::new());
    assert!(changed);
    assert_eq!(entry, json!({"additional_directories": ["/b"]}));
}

#[test]
fn adding_counts_unseen_entries_and_command_line_roots_against_the_limit() {
    let unseen: Vec<String> = (0..MAX_ADDITIONAL_DIRECTORIES)
        .map(|index| format!("/d{index}"))
        .collect();
    let mut entry = json!({ "additional_directories": unseen });
    let patch = DurablePatch {
        change: Patch::Add("/b".to_owned()),
        observed: Vec::new(),
        command_line: Vec::new(),
    };
    assert_eq!(
        apply(&mut entry, &patch),
        Err(CommandError::Access(
            WorkspaceAccessError::TooManyDirectories
        ))
    );
    let saved: Vec<String> = (1..MAX_ADDITIONAL_DIRECTORIES)
        .map(|index| format!("/d{index}"))
        .collect();
    let mut entry = json!({ "additional_directories": saved });
    let patch = DurablePatch {
        change: Patch::Add("/b".to_owned()),
        observed: Vec::new(),
        command_line: vec!["/launch".to_owned(), "/d1".to_owned()],
    };
    assert_eq!(
        apply(&mut entry, &patch),
        Err(CommandError::Access(
            WorkspaceAccessError::TooManyDirectories
        ))
    );
}

#[test]
fn removing_drops_every_spelling_of_the_identity_and_the_empty_key() {
    let pairs = [("/a/./x", "/a/x"), ("/b", "/b")];
    let (entry, changed) = patched(
        json!({"additional_directories": ["/a/./x", "/b"]}),
        Patch::Remove("/a/x".to_owned()),
        observed(&pairs),
    );
    assert!(changed);
    assert_eq!(entry, json!({"additional_directories": ["/b"]}));
    let (entry, changed) = patched(
        json!({"additional_directories": ["/b"], "model": "m"}),
        Patch::Remove("/b".to_owned()),
        observed(&pairs),
    );
    assert!(changed);
    assert_eq!(entry, json!({"model": "m"}));
    let (_, changed) = patched(
        json!({"additional_directories": ["/b"]}),
        Patch::Remove("/c".to_owned()),
        observed(&pairs),
    );
    assert!(!changed);
}

#[test]
fn malformed_entries_are_refused() {
    for entry in [
        json!([1]),
        json!({"additional_directories": "x"}),
        json!({"additional_directories": [1]}),
    ] {
        let mut entry = entry;
        let patch = DurablePatch {
            change: Patch::Add("/b".to_owned()),
            observed: Vec::new(),
            command_line: Vec::new(),
        };
        assert_eq!(
            apply(&mut entry, &patch),
            Err(CommandError::InvalidSettingsFormat)
        );
    }
}

#[test]
fn adding_saves_the_real_path_and_reports_the_runtime_change() {
    let fixture = Fixture::new();
    let shared = fixture.directory("shared");
    let current = WorkspaceAccess::primary_only(&fixture.primary);
    let (access, mutation) = updated(
        execute(
            Some(&fixture.paths),
            &current,
            &Action::Add("../shared".to_owned()),
        )
        .unwrap(),
    );
    assert_eq!(
        mutation,
        Mutation {
            action: "add",
            path: Some("../shared".to_owned()),
            saved_changed: true,
            runtime_changed: true,
            launch_flag_can_restore: false,
        }
    );
    assert_eq!(
        access.active_roots().collect::<Vec<_>>(),
        [shared.as_path()]
    );
    assert_eq!(fixture.durable(), [text(&shared)]);
}

#[test]
fn stale_actions_apply_to_the_latest_durable_roots() {
    let fixture = Fixture::new();
    let added = fixture.directory("added");
    let launch = fixture.directory("launch");
    let saved = fixture.saved_directories(MAX_ADDITIONAL_DIRECTORIES - 1);
    fixture.write_saved(&saved);
    let first = launched(&fixture.primary, &saved, &[&launch]);
    let stale_second = first.clone();
    updated(
        execute(
            Some(&fixture.paths),
            &first,
            &Action::Remove(saved[0].clone()),
        )
        .unwrap(),
    );
    let (access, _) = updated(
        execute(
            Some(&fixture.paths),
            &stale_second,
            &Action::Add(text(&added)),
        )
        .unwrap(),
    );
    let last = access.entries().last().unwrap();
    assert_eq!(access.entries().len(), MAX_ADDITIONAL_DIRECTORIES);
    assert_eq!(last.path, launch);
    assert!(last.source.command_line);
    let durable = fixture.durable();
    assert_eq!(durable.len(), MAX_ADDITIONAL_DIRECTORIES - 1);
    assert_eq!(durable[0], saved[1]);
    assert_eq!(durable.last(), Some(&text(&added)));
}

#[test]
fn adding_refuses_effective_capacity_before_changing_settings() {
    let fixture = Fixture::new();
    let added = fixture.directory("added");
    let launch = fixture.directory("launch");
    let saved = fixture.saved_directories(MAX_ADDITIONAL_DIRECTORIES - 1);
    fixture.write_saved(&saved);
    let before = fs::read(fixture.settings_path()).unwrap();
    let current = launched(&fixture.primary, &saved, &[&launch]);
    assert_eq!(
        execute(Some(&fixture.paths), &current, &Action::Add(text(&added))),
        Err(failed(
            FailurePhase::Commit,
            WorkspaceAccessError::TooManyDirectories
        ))
    );
    assert_eq!(fs::read(fixture.settings_path()).unwrap(), before);
}

#[test]
fn removing_command_line_only_access_writes_nothing_and_warns_the_flag_can_restore_it() {
    let fixture = Fixture::new();
    let launch = fixture.directory("launch");
    let current = launched(&fixture.primary, &[], &[&launch]);
    let (access, mutation) = updated(
        execute(
            Some(&fixture.paths),
            &current,
            &Action::Remove(text(&launch)),
        )
        .unwrap(),
    );
    assert!(!mutation.saved_changed);
    assert!(mutation.runtime_changed);
    assert!(mutation.launch_flag_can_restore);
    assert!(access.entries().is_empty());
    assert!(!fixture.settings_path().exists());
}

#[test]
fn failures_report_the_transaction_phase_they_stopped_in() {
    let fixture = Fixture::new();
    let shared = fixture.directory("shared");
    let current = WorkspaceAccess::primary_only(&fixture.primary);
    assert_eq!(
        execute(None, &current, &Action::Add(text(&fixture.primary))),
        Err(failed(
            FailurePhase::Stage,
            WorkspaceAccessError::PrimaryDirectory
        ))
    );
    assert_eq!(
        execute(None, &current, &Action::Remove(text(&shared))),
        Err(failed(
            FailurePhase::Stage,
            WorkspaceAccessError::UnknownAdditionalDirectory
        ))
    );
    assert_eq!(
        execute(None, &current, &Action::Add(text(&shared))),
        Err(failed(FailurePhase::Commit, CommandError::HomeNotSet))
    );
    fixture.write_saved(&["relative".to_owned()]);
    assert_eq!(
        execute(Some(&fixture.paths), &current, &Action::Add(text(&shared))),
        Err(failed(
            FailurePhase::Reconcile,
            CommandError::InvalidSettingsFormat
        ))
    );
    assert_eq!(
        failed(FailurePhase::Commit, CommandError::HomeNotSet).to_string(),
        "HomeNotSet"
    );
}

#[test]
fn reconciliation_accepts_only_the_intended_or_previous_saved_state() {
    let fixture = Fixture::new();
    let previous = fixture.directory("previous");
    let launch = fixture.directory("launch");
    let third = fixture.directory("third");
    let current = launched(&fixture.primary, &[text(&previous)], &[&launch]);
    let intended = current.stage_clear();
    let classified = || reconcile(&fixture.paths, &current, Some(&intended)).unwrap();
    fixture.write_saved(&[]);
    assert_eq!(classified(), Reconciliation::Intended(intended.clone()));
    fixture.write_saved(&[text(&previous)]);
    assert_eq!(classified(), Reconciliation::Previous(current.clone()));
    fixture.write_saved(&[text(&third)]);
    assert_eq!(classified(), Reconciliation::Unconfirmed);
    fixture.write_saved(&[text(&previous), text(&previous)]);
    assert_eq!(classified(), Reconciliation::Unconfirmed);
    assert_eq!(
        reconcile(&fixture.paths, &current, None),
        Ok(Reconciliation::Unconfirmed)
    );
}

#[test]
fn reconciliation_rejects_a_retargeted_durable_source() {
    let fixture = Fixture::new();
    let first = fixture.directory("first");
    let second = fixture.directory("second");
    let link = fixture.root.join("saved-link");
    symlink(&first, &link).unwrap();
    let current = WorkspaceAccess::new(&fixture.primary, &[text(&link)]).unwrap();
    let intended = current.stage_clear();
    fixture.write_saved(&[text(&link)]);
    fs::remove_file(&link).unwrap();
    symlink(&second, &link).unwrap();
    assert_eq!(
        reconcile(&fixture.paths, &current, Some(&intended)),
        Ok(Reconciliation::Unconfirmed)
    );
}
