use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;

use ofx_permissions::prepare_file_mutation_targets;
use tempfile::TempDir;

use super::*;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        Self {
            _temp: temp,
            root,
            workspace,
        }
    }

    fn write(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    fn targets(&self, path: &str) -> FileMutationTargets {
        prepare_file_mutation_targets(&self.workspace, path, FileMutationKind::Write).unwrap()
    }

    fn prepare(&self, path: &str, content: &str) -> Result<PreparedMutation, PrepareFailure> {
        PreparedMutation::prepare(
            self.targets(path),
            path,
            &MutationInput::Write(content.to_owned()),
        )
    }

    fn prepared(&self, path: &str, content: &str) -> PreparedMutation {
        self.prepare(path, content).unwrap()
    }
}

fn apply(prepared: &PreparedMutation) -> Result<(), Rejection> {
    prepared.apply(&CancellationToken::new())
}

fn stage_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() && !path.is_symlink() {
                pending.push(path.clone());
            }
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(STAGE_PREFIX))
            {
                found.push(path);
            }
        }
    }
    found
}

fn semantic(failure: PrepareFailure) -> String {
    match failure {
        PrepareFailure::Semantic(message) => message,
        PrepareFailure::Operational(error) => panic!("operational {error}"),
    }
}

#[test]
fn a_missing_write_is_prepared_without_creating_the_target_or_its_parents() {
    let fixture = Fixture::new();
    let prepared = fixture.prepared("missing/parents/new.txt", "hello\n");

    assert!(prepared.creates_file());
    assert!(!prepared.is_noop());
    assert_eq!(prepared.after, b"hello\n");
    assert_eq!(prepared.display_path, "missing/parents/new.txt");
    assert!(!fixture.workspace.join("missing").exists());
}

#[test]
fn an_existing_write_keeps_the_exact_reviewed_preimage() {
    let fixture = Fixture::new();
    fixture.write("workspace/note.txt", "old\n");
    let prepared = fixture.prepared("note.txt", "new\n");

    assert_eq!(
        prepared.preimage,
        Preimage::Present {
            content: b"old\n".to_vec(),
            hash: Sha256::digest(b"old\n").to_vec(),
        }
    );
    assert!(!prepared.creates_file());
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("note.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn identical_content_is_a_no_op_named_by_the_requested_path() {
    let fixture = Fixture::new();
    fixture.write("workspace/same.txt", "same\n");
    let prepared = fixture.prepared(" same.txt", "same\n");

    assert!(prepared.is_noop());
    assert_eq!(
        prepared.noop_message(),
        "No changes to  same.txt; it already contains the requested content"
    );
    assert!(!fixture.prepared("empty.txt", "").is_noop());
}

#[test]
fn external_targets_display_the_canonical_path_behind_a_symlink() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("outside")).unwrap();
    symlink("../outside", fixture.workspace.join("link")).unwrap();
    let misleading = fixture.workspace.join("link/new.txt");
    let prepared = fixture.prepared(misleading.to_str().unwrap(), "hello\n");

    assert!(prepared.targets().target.anchor_is_external);
    let canonical = fixture.root.join("outside/new.txt");
    assert_eq!(prepared.display_path, canonical.to_str().unwrap());
    apply(&prepared).unwrap();
    assert_eq!(
        prepared.success_message(),
        format!("wrote {} (6 bytes)", canonical.display())
    );
    assert_eq!(fs::read_to_string(canonical).unwrap(), "hello\n");
}

#[test]
fn hostile_paths_are_displayed_terminal_safe() {
    let fixture = Fixture::new();
    let hostile = "name\u{1b}[31m\nfile.txt";
    fixture.write(&format!("workspace/{hostile}"), "same");
    let prepared = fixture.prepared(hostile, "same");

    let message = prepared.noop_message();
    assert!(
        !message.contains('\u{1b}') && !message.contains('\n'),
        "{message}"
    );
    let changed = fixture.prepared(hostile, "new");
    apply(&changed).unwrap();
    let message = changed.success_message();
    assert!(
        !message.contains('\u{1b}') && !message.contains('\n'),
        "{message}"
    );
}

#[test]
fn oversized_preimages_and_non_regular_targets_fail_before_approval() {
    let fixture = Fixture::new();
    fixture.write("workspace/large.txt", &"x".repeat(MAX_CONTENT_BYTES + 1));
    fs::create_dir(fixture.workspace.join("directory")).unwrap();
    fixture.write("outside.txt", "outside");
    symlink(
        fixture.root.join("outside.txt"),
        fixture.workspace.join("link.txt"),
    )
    .unwrap();

    assert_eq!(
        semantic(fixture.prepare("large.txt", "new").unwrap_err()),
        PREIMAGE_TOO_LARGE
    );
    for path in ["directory", "link.txt"] {
        assert_eq!(
            semantic(fixture.prepare(path, "new").unwrap_err()),
            NOT_REGULAR_FILE,
            "{path}"
        );
    }
    assert_eq!(
        fs::read_to_string(fixture.root.join("outside.txt")).unwrap(),
        "outside"
    );
}

#[test]
fn a_parent_retargeted_after_target_resolution_fails_preparation() {
    let fixture = Fixture::new();
    fixture.write("workspace/src/note.txt", "old");
    fixture.write("outside/note.txt", "outside");
    let targets = fixture.targets("src/note.txt");
    fs::rename(
        fixture.workspace.join("src"),
        fixture.workspace.join("src.moved"),
    )
    .unwrap();
    symlink(fixture.root.join("outside"), fixture.workspace.join("src")).unwrap();

    let failure = PreparedMutation::prepare(
        targets,
        "src/note.txt",
        &MutationInput::Write("new".to_owned()),
    )
    .unwrap_err();

    assert_eq!(semantic(failure), IDENTITY_CHANGED);
    assert_eq!(
        fs::read_to_string(fixture.root.join("outside/note.txt")).unwrap(),
        "outside"
    );
}

#[test]
fn apply_installs_the_reviewed_bytes_and_creates_missing_parents() {
    let fixture = Fixture::new();
    let prepared = fixture.prepared("a/b/new.txt", "hello\n");

    apply(&prepared).unwrap();

    assert_eq!(
        fs::read_to_string(fixture.workspace.join("a/b/new.txt")).unwrap(),
        "hello\n"
    );
    assert_eq!(prepared.success_message(), "wrote a/b/new.txt (6 bytes)");
    assert!(stage_files(&fixture.root).is_empty());
}

#[test]
fn apply_replaces_an_existing_file_and_keeps_its_mode() {
    let fixture = Fixture::new();
    let path = fixture.write("workspace/run.sh", "echo old\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
    let prepared = fixture.prepared("run.sh", "echo new\n");

    apply(&prepared).unwrap();

    assert_eq!(fs::read_to_string(&path).unwrap(), "echo new\n");
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o750);
}

#[test]
fn apply_splits_a_hard_link_instead_of_writing_through_it() {
    let fixture = Fixture::new();
    let outside = fixture.write("outside.txt", "outside\n");
    fs::hard_link(&outside, fixture.workspace.join("linked.txt")).unwrap();
    let prepared = fixture.prepared("linked.txt", "inside\n");

    apply(&prepared).unwrap();

    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside\n");
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("linked.txt")).unwrap(),
        "inside\n"
    );
}

#[test]
fn apply_rejects_targets_that_changed_after_preparation() {
    let fixture = Fixture::new();
    let created_later = fixture.prepared("later.txt", "mine\n");
    fixture.write("workspace/later.txt", "theirs\n");
    let edited_later = {
        fixture.write("workspace/note.txt", "old\n");
        fixture.prepared("note.txt", "mine\n")
    };
    fixture.write("workspace/note.txt", "theirs\n");

    for prepared in [&created_later, &edited_later] {
        let rejection = apply(prepared).unwrap_err();
        assert_eq!(rejection.reason, RejectReason::StalePreimage);
        assert_eq!(
            rejection.message(),
            "file mutation rejected because the file changed after preview; make a new tool call for a fresh preview"
        );
    }
    for name in ["later.txt", "note.txt"] {
        assert_eq!(
            fs::read_to_string(fixture.workspace.join(name)).unwrap(),
            "theirs\n"
        );
    }
    assert!(stage_files(&fixture.root).is_empty());
}

#[test]
fn apply_refuses_a_parent_swapped_for_a_symlink_after_preparation() {
    let fixture = Fixture::new();
    fixture.write("workspace/src/note.txt", "old\n");
    fs::create_dir(fixture.root.join("outside")).unwrap();
    let prepared = fixture.prepared("src/note.txt", "new\n");
    let created = fixture.prepared("src/fresh/new.txt", "new\n");
    fs::rename(
        fixture.workspace.join("src"),
        fixture.workspace.join("src.moved"),
    )
    .unwrap();
    symlink(fixture.root.join("outside"), fixture.workspace.join("src")).unwrap();

    for prepared in [&prepared, &created] {
        let rejection = apply(prepared).unwrap_err();
        assert_eq!(rejection.reason, RejectReason::TraversalChanged);
        assert_eq!(
            rejection.message(),
            "file mutation rejected because the approved path traversal changed"
        );
    }
    assert_eq!(
        fs::read_dir(fixture.root.join("outside")).unwrap().count(),
        0
    );
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("src.moved/note.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn apply_refuses_a_replaced_anchor() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("external")).unwrap();
    let target = fixture.root.join("external/new.txt");
    let prepared = fixture.prepared(target.to_str().unwrap(), "new\n");
    fs::rename(
        fixture.root.join("external"),
        fixture.root.join("external.moved"),
    )
    .unwrap();
    fs::create_dir(fixture.root.join("external")).unwrap();

    assert_eq!(
        apply(&prepared).unwrap_err().reason,
        RejectReason::TraversalChanged
    );
    assert!(!target.exists());
}

#[test]
fn read_only_targets_fail_at_apply_without_changing_them() {
    let fixture = Fixture::new();
    let path = fixture.write("workspace/locked.txt", "old\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    let prepared = fixture.prepared("locked.txt", "new\n");

    let rejection = apply(&prepared).unwrap_err();

    assert_eq!(rejection.reason, RejectReason::IoFailure);
    assert_eq!(rejection.message(), "file mutation failed before commit");
    assert_eq!(fs::read_to_string(&path).unwrap(), "old\n");
}

#[test]
fn cancellation_before_commit_writes_nothing() {
    let fixture = Fixture::new();
    let prepared = fixture.prepared("dir/new.txt", "new\n");
    let cancel = CancellationToken::new();
    cancel.cancel();

    let rejection = prepared.apply(&cancel).unwrap_err();

    assert_eq!(rejection.message(), "file mutation cancelled before commit");
    assert!(!fixture.workspace.join("dir").exists());
}

#[test]
fn rejected_transactions_remove_the_parents_they_created_or_report_them() {
    let fixture = Fixture::new();
    let prepared = fixture.prepared("a/b/new.txt", "new\n");

    let mut transaction = Transaction::new(&prepared);
    drop(transaction.realize_traversal().unwrap());
    assert!(fixture.workspace.join("a/b").is_dir());
    let rejection = transaction.reject(RejectReason::IoFailure);
    assert_eq!(rejection.message(), "file mutation failed before commit");
    assert!(!fixture.workspace.join("a").exists());

    let mut transaction = Transaction::new(&prepared);
    drop(transaction.realize_traversal().unwrap());
    fs::write(fixture.workspace.join("a/b/other.txt"), "other").unwrap();
    let rejection = transaction.reject(RejectReason::Cancelled);
    assert_eq!(
        rejection.message(),
        format!(
            "file mutation cancelled before commit; approved parent cleanup residue: {} (not_empty) {} (not_empty)",
            fixture.workspace.join("a/b").display(),
            fixture.workspace.join("a").display()
        )
    );

    fs::remove_dir_all(fixture.workspace.join("a")).unwrap();
    let mut transaction = Transaction::new(&prepared);
    drop(transaction.realize_traversal().unwrap());
    fs::rename(
        fixture.workspace.join("a/b"),
        fixture.workspace.join("a/b.moved"),
    )
    .unwrap();
    fs::create_dir(fixture.workspace.join("a/b")).unwrap();
    let rejection = transaction.reject(RejectReason::IoFailure);
    assert_eq!(
        rejection.message(),
        format!(
            "file mutation failed before commit; approved parent cleanup residue: {} (identity_changed) {} (not_empty)",
            fixture.workspace.join("a/b").display(),
            fixture.workspace.join("a").display()
        )
    );
}

#[test]
fn a_directory_created_where_a_missing_parent_was_planned_fails_the_traversal() {
    let fixture = Fixture::new();
    let prepared = fixture.prepared("a/new.txt", "new\n");
    fs::create_dir(fixture.workspace.join("a")).unwrap();

    assert_eq!(
        apply(&prepared).unwrap_err().reason,
        RejectReason::TraversalChanged
    );
    assert!(!fixture.workspace.join("a/new.txt").exists());
}

#[test]
fn large_content_is_staged_in_chunks_and_installed_whole() {
    let fixture = Fixture::new();
    let content = "0123456789abcdef".repeat(WRITE_CHUNK_BYTES / 4);
    let prepared = fixture.prepared("large.txt", &content);

    apply(&prepared).unwrap();

    assert_eq!(
        fs::read_to_string(fixture.workspace.join("large.txt")).unwrap(),
        content
    );
}

fn apply_at(
    prepared: &PreparedMutation,
    at: Checkpoint,
    mut action: impl FnMut(),
) -> Result<(), Rejection> {
    prepared.apply_with(&CancellationToken::new(), &mut |checkpoint| {
        if checkpoint == at {
            action();
        }
    })
}

fn stage_file(directory: &Path) -> PathBuf {
    let mut stages = stage_files(directory);
    assert_eq!(stages.len(), 1, "{stages:?}");
    stages.remove(0)
}

#[test]
fn same_size_content_changes_are_caught_by_the_content_hash() {
    let fixture = Fixture::new();
    let path = fixture.write("workspace/note.txt", "old\n");
    let before_apply = fixture.prepared("note.txt", "new\n");
    fs::write(&path, "OLD\n").unwrap();
    assert_eq!(
        apply(&before_apply).unwrap_err().reason,
        RejectReason::StalePreimage
    );

    fs::write(&path, "old\n").unwrap();
    let after_staging = fixture.prepared("note.txt", "new\n");
    let rejection = apply_at(&after_staging, Checkpoint::Staged, || {
        fs::write(&path, "OLD\n").unwrap();
    })
    .unwrap_err();
    assert_eq!(rejection.reason, RejectReason::StalePreimage);
    assert_eq!(fs::read_to_string(&path).unwrap(), "OLD\n");
    assert!(stage_files(&fixture.root).is_empty());
}

#[test]
fn a_mode_change_after_staging_is_stale() {
    let fixture = Fixture::new();
    let path = fixture.write("workspace/note.txt", "old\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let prepared = fixture.prepared("note.txt", "new\n");

    let rejection = apply_at(&prepared, Checkpoint::Staged, || {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    })
    .unwrap_err();

    assert_eq!(rejection.reason, RejectReason::StalePreimage);
    assert_eq!(fs::read_to_string(&path).unwrap(), "old\n");
}

#[test]
fn a_tampered_stage_file_is_rejected_and_removed() {
    let fixture = Fixture::new();
    let prepared = fixture.prepared("note.txt", "mine\n");
    let workspace = fixture.workspace.clone();

    let rejection = apply_at(&prepared, Checkpoint::Staged, || {
        fs::write(stage_file(&workspace), "evil\n").unwrap();
    })
    .unwrap_err();

    assert_eq!(rejection.reason, RejectReason::StagedSourceChanged);
    assert_eq!(
        rejection.message(),
        "file mutation rejected because the staged file changed before commit"
    );
    assert!(!fixture.workspace.join("note.txt").exists());
    assert!(stage_files(&fixture.root).is_empty());
}

#[test]
fn a_replaced_stage_file_is_rejected_without_deleting_the_replacement() {
    let fixture = Fixture::new();
    let prepared = fixture.prepared("note.txt", "mine\n");
    let workspace = fixture.workspace.clone();
    let mut replacement = PathBuf::new();

    let rejection = apply_at(&prepared, Checkpoint::Staged, || {
        let stage = stage_file(&workspace);
        fs::rename(&stage, workspace.join("moved-stage")).unwrap();
        fs::write(&stage, "mine\n").unwrap();
        replacement = stage;
    })
    .unwrap_err();

    assert_eq!(rejection.reason, RejectReason::StagedSourceChanged);
    assert_eq!(fs::read_to_string(&replacement).unwrap(), "mine\n");
    assert!(!fixture.workspace.join("note.txt").exists());
}

#[test]
fn a_parent_swapped_for_a_symlink_after_staging_fails_the_traversal() {
    let fixture = Fixture::new();
    fixture.write("workspace/src/note.txt", "old\n");
    fs::create_dir(fixture.root.join("outside")).unwrap();
    let prepared = fixture.prepared("src/note.txt", "new\n");
    let (workspace, outside) = (fixture.workspace.clone(), fixture.root.join("outside"));

    let rejection = apply_at(&prepared, Checkpoint::Staged, || {
        fs::rename(workspace.join("src"), workspace.join("src.moved")).unwrap();
        symlink(&outside, workspace.join("src")).unwrap();
    })
    .unwrap_err();

    assert_eq!(rejection.reason, RejectReason::TraversalChanged);
    assert_eq!(
        fs::read_dir(fixture.root.join("outside")).unwrap().count(),
        0
    );
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("src.moved/note.txt")).unwrap(),
        "old\n"
    );
    assert!(stage_files(&fixture.root).is_empty());
}

#[test]
fn a_parent_moved_after_final_validation_still_receives_the_commit() {
    let fixture = Fixture::new();
    fixture.write("workspace/src/note.txt", "old\n");
    let prepared = fixture.prepared("src/note.txt", "new\n");
    let workspace = fixture.workspace.clone();

    apply_at(&prepared, Checkpoint::Validated, || {
        fs::rename(workspace.join("src"), workspace.join("src.moved")).unwrap();
        fs::create_dir(workspace.join("src")).unwrap();
    })
    .unwrap();

    assert_eq!(
        fs::read_to_string(fixture.workspace.join("src.moved/note.txt")).unwrap(),
        "new\n"
    );
    assert!(!fixture.workspace.join("src/note.txt").exists());
}

#[test]
fn a_file_created_after_final_validation_is_not_replaced() {
    let fixture = Fixture::new();
    let prepared = fixture.prepared("new.txt", "mine\n");
    let path = fixture.workspace.join("new.txt");

    let rejection = apply_at(&prepared, Checkpoint::Validated, || {
        fs::write(&path, "theirs\n").unwrap();
    })
    .unwrap_err();

    assert_eq!(rejection.reason, RejectReason::StalePreimage);
    assert_eq!(fs::read_to_string(&path).unwrap(), "theirs\n");
    assert!(stage_files(&fixture.root).is_empty());
}

#[test]
fn cancellation_inside_the_transaction_removes_the_stage_and_created_parents() {
    let fixture = Fixture::new();
    let prepared = fixture.prepared("a/b/new.txt", "new\n");
    let cancel = CancellationToken::new();

    let rejection = prepared
        .apply_with(&cancel, &mut |checkpoint| {
            if checkpoint == Checkpoint::Staged {
                cancel.cancel();
            }
        })
        .unwrap_err();

    assert_eq!(rejection.message(), "file mutation cancelled before commit");
    assert!(!fixture.workspace.join("a").exists());
    assert!(stage_files(&fixture.root).is_empty());
}
