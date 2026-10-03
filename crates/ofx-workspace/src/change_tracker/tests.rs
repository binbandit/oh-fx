use std::ffi::{OsStr, OsString};
use std::fs::{self, Permissions};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use tempfile::TempDir;

use super::*;
use crate::pathing::{descriptor_identity, open_child_directory, open_directory};

const PERMISSION_BITS: u32 = 0o7777;

struct Root {
    _temp: TempDir,
    path: PathBuf,
}

impl Root {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = fs::canonicalize(temp.path()).unwrap();
        Self { _temp: temp, path }
    }

    fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }

    fn capture(&self, relative: &str, previous_content: Option<&str>) -> FileOperation {
        captured(&self.path, relative, previous_content)
    }
}

fn captured(anchor: &Path, relative: &str, previous_content: Option<&str>) -> FileOperation {
    let components: Vec<OsString> = Path::new(relative).iter().map(OsStr::to_owned).collect();
    let mut current = open_directory(anchor).unwrap();
    let anchor_identity = descriptor_identity(&current).unwrap();
    let mut parent_identities = Vec::new();
    for component in &components[..components.len() - 1] {
        current = open_child_directory(&current, component).unwrap();
        parent_identities.push(descriptor_identity(&current).unwrap());
    }
    FileOperation {
        target: FileMutationTarget {
            anchor: anchor.to_owned(),
            anchor_is_external: false,
            components,
        },
        anchor_identity,
        parent_identities,
        previous_content: previous_content.map(|content| content.as_bytes().to_vec()),
    }
}

fn placeholder(path: &str) -> FileOperation {
    let root = descriptor_identity(open_directory(Path::new("/")).unwrap()).unwrap();
    let components: Vec<OsString> = Path::new(path)
        .iter()
        .skip(1)
        .map(OsStr::to_owned)
        .collect();
    FileOperation {
        parent_identities: vec![root; components.len() - 1],
        target: FileMutationTarget {
            anchor: PathBuf::from("/"),
            anchor_is_external: false,
            components,
        },
        anchor_identity: root,
        previous_content: None,
    }
}

fn tracked(tracker: &ChangeTracker) -> Vec<PathBuf> {
    tracker
        .stack()
        .iter()
        .map(|operation| operation.target.path())
        .collect()
}

fn lock(directory: &Path) -> bool {
    fs::set_permissions(directory, Permissions::from_mode(0o500)).unwrap();
    let probe = directory.join(".write-probe");
    let denied = fs::write(&probe, b"").is_err();
    let _ = fs::remove_file(probe);
    denied
}

fn unlock(directory: &Path) {
    fs::set_permissions(directory, Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn undo_last_returns_empty_on_an_initially_empty_stack() {
    assert_eq!(ChangeTracker::default().undo_last(), UndoResult::Empty);
}

#[test]
fn clear_releases_operations_and_leaves_the_tracker_empty() {
    let tracker = ChangeTracker::default();
    tracker.push_operation(placeholder("/workspace/a.txt"));
    tracker.clear();
    assert!(tracked(&tracker).is_empty());
    assert_eq!(tracker.undo_last(), UndoResult::Empty);
    tracker.push_operation(placeholder("/workspace/reused.txt"));
    assert_eq!(tracked(&tracker), [PathBuf::from("/workspace/reused.txt")]);
}

#[test]
fn push_operation_evicts_the_oldest_operation_with_stable_ordering() {
    let tracker = ChangeTracker::default();
    for index in 0..=MAX_STACK_SIZE {
        tracker.push_operation(placeholder(&format!("/tracked/file-{index}")));
    }
    let paths = tracked(&tracker);
    assert_eq!(paths.len(), MAX_STACK_SIZE);
    assert_eq!(paths[0], Path::new("/tracked/file-1"));
    assert_eq!(paths[MAX_STACK_SIZE - 1], Path::new("/tracked/file-100"));
}

#[test]
fn clones_share_one_stack() {
    let tracker = ChangeTracker::default();
    tracker
        .clone()
        .push_operation(placeholder("/workspace/shared.txt"));
    assert_eq!(tracked(&tracker), [PathBuf::from("/workspace/shared.txt")]);
}

#[test]
fn undo_last_restores_previous_content_for_write_and_edit_operations() {
    let root = Root::new();
    let path = root.join("restore.txt");
    fs::write(&path, "modified").unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(root.capture("restore.txt", Some("original")));
    assert_eq!(tracker.undo_last(), UndoResult::Restored(path.clone()));
    assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    assert_eq!(fs::read_dir(&root.path).unwrap().count(), 1);
}

#[test]
fn undo_last_restores_the_newest_operation_first() {
    let root = Root::new();
    let path = root.join("twice.txt");
    fs::write(&path, "third").unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(root.capture("twice.txt", Some("first")));
    tracker.push_operation(root.capture("twice.txt", Some("second")));
    assert_eq!(tracker.undo_last(), UndoResult::Restored(path.clone()));
    assert_eq!(fs::read_to_string(&path).unwrap(), "second");
    assert_eq!(tracker.undo_last(), UndoResult::Restored(path.clone()));
    assert_eq!(fs::read_to_string(&path).unwrap(), "first");
    assert_eq!(tracker.undo_last(), UndoResult::Empty);
}

#[test]
fn a_restored_file_keeps_the_permissions_of_the_file_it_replaces() {
    let root = Root::new();
    let path = root.join("script.sh");
    fs::write(&path, "changed").unwrap();
    fs::set_permissions(&path, Permissions::from_mode(0o700)).unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(root.capture("script.sh", Some("original")));
    assert_eq!(tracker.undo_last(), UndoResult::Restored(path.clone()));
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & PERMISSION_BITS, 0o700);
}

#[test]
fn undo_last_deletes_new_write_and_edit_operations() {
    let root = Root::new();
    let path = root.join("new.txt");
    fs::write(&path, "new content").unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(root.capture("new.txt", None));
    assert_eq!(tracker.undo_last(), UndoResult::Deleted(path.clone()));
    assert!(!path.exists());
}

#[test]
fn undo_last_reports_deleted_for_a_new_write_when_the_file_is_already_absent() {
    let root = Root::new();
    let path = root.join("already-absent.txt");
    let tracker = ChangeTracker::default();
    tracker.push_operation(root.capture("already-absent.txt", None));
    assert_eq!(tracker.undo_last(), UndoResult::Deleted(path.clone()));
    assert!(!path.exists());
}

#[test]
fn undo_last_reports_unavailable_when_a_new_file_cannot_be_deleted() {
    let root = Root::new();
    let locked = root.join("locked");
    fs::create_dir(&locked).unwrap();
    let path = locked.join("new.txt");
    fs::write(&path, "new content").unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(root.capture("locked/new.txt", None));
    let denied = lock(&locked);
    let result = tracker.undo_last();
    unlock(&locked);
    if !denied {
        return;
    }
    assert_eq!(result, UndoResult::Unavailable(path.clone()));
    assert!(tracked(&tracker).is_empty());
    assert_eq!(fs::read_to_string(&path).unwrap(), "new content");
}

#[test]
fn undo_last_reports_unavailable_when_a_created_path_is_no_longer_a_file() {
    let root = Root::new();
    let path = root.join("now-a-directory");
    fs::create_dir(&path).unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(root.capture("now-a-directory", None));
    assert_eq!(tracker.undo_last(), UndoResult::Unavailable(path.clone()));
    assert!(path.is_dir());
}

#[test]
fn undo_last_pops_before_filesystem_restore_failures_reports_them_and_does_not_create_parents() {
    let root = Root::new();
    let parent = root.join("missing-parent");
    fs::create_dir(&parent).unwrap();
    let path = parent.join("file.txt");
    let tracker = ChangeTracker::default();
    tracker.push_operation(root.capture("missing-parent/file.txt", Some("content")));
    fs::remove_dir(&parent).unwrap();
    assert_eq!(tracker.undo_last(), UndoResult::Unavailable(path.clone()));
    assert!(tracked(&tracker).is_empty());
    assert!(!path.exists());
    assert!(!parent.exists());
    assert_eq!(tracker.undo_last(), UndoResult::Empty);
}

#[test]
fn undo_last_leaves_a_file_without_write_permission_intact() {
    let root = Root::new();
    let path = root.join("read-only.txt");
    fs::write(&path, "bytes the user still has on disk").unwrap();
    fs::set_permissions(&path, Permissions::from_mode(0o444)).unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(root.capture("read-only.txt", Some("preimage bytes")));
    assert_eq!(tracker.undo_last(), UndoResult::Unavailable(path.clone()));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "bytes the user still has on disk"
    );
    assert_eq!(fs::read_dir(&root.path).unwrap().count(), 1);
}

#[test]
fn undo_refuses_a_file_whose_directory_denies_writes_and_leaves_it_intact() {
    let root = Root::new();
    let locked = root.join("locked");
    fs::create_dir(&locked).unwrap();
    let path = locked.join("file.txt");
    let current = "x".repeat(64 * 1024);
    fs::write(&path, &current).unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(root.capture("locked/file.txt", Some("preimage bytes")));
    let denied = lock(&locked);
    let result = tracker.undo_last();
    unlock(&locked);
    if !denied {
        return;
    }
    assert_eq!(result, UndoResult::Unavailable(path.clone()));
    assert_eq!(fs::read_to_string(&path).unwrap(), current);
    assert_eq!(fs::read_dir(&locked).unwrap().count(), 1);
}

#[test]
fn undo_restore_cannot_be_redirected_by_a_symlink_introduced_after_capture() {
    let root = Root::new();
    let recorded = root.join("recorded.txt");
    let redirect_target = root.join("redirect-target.txt");
    fs::write(&recorded, "current bytes").unwrap();
    fs::write(&redirect_target, "must stay untouched").unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(root.capture("recorded.txt", Some("preimage bytes")));
    fs::remove_file(&recorded).unwrap();
    symlink(&redirect_target, &recorded).unwrap();
    assert_eq!(tracker.undo_last(), UndoResult::Restored(recorded.clone()));
    assert!(fs::symlink_metadata(&recorded).unwrap().is_file());
    assert_eq!(fs::read_to_string(&recorded).unwrap(), "preimage bytes");
    assert_eq!(
        fs::read_to_string(&redirect_target).unwrap(),
        "must stay untouched"
    );
}

fn swap_parent_for_outside_symlink(root: &Root) {
    fs::rename(root.join("workspace/sub"), root.join("workspace/saved-sub")).unwrap();
    symlink(root.join("outside"), root.join("workspace/sub")).unwrap();
}

fn swapped_parent_fixture(current: &str) -> (Root, FileOperation, PathBuf) {
    let root = Root::new();
    fs::create_dir_all(root.join("workspace/sub")).unwrap();
    fs::create_dir(root.join("outside")).unwrap();
    let path = root.join("workspace/sub/note");
    fs::write(&path, current).unwrap();
    fs::write(root.join("outside/note"), "outside bytes").unwrap();
    let operation = captured(&root.join("workspace"), "sub/note", None);
    (root, operation, path)
}

#[test]
fn undo_refuses_a_restore_through_a_parent_swapped_for_an_outside_symlink() {
    let (root, mut operation, path) = swapped_parent_fixture("changed");
    operation.previous_content = Some(b"original".to_vec());
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation);
    swap_parent_for_outside_symlink(&root);
    assert_eq!(tracker.undo_last(), UndoResult::Unavailable(path));
    assert_eq!(
        fs::read_to_string(root.join("outside/note")).unwrap(),
        "outside bytes"
    );
    assert_eq!(fs::read_dir(root.join("outside")).unwrap().count(), 1);
    assert_eq!(
        fs::read_to_string(root.join("workspace/saved-sub/note")).unwrap(),
        "changed"
    );
}

#[test]
fn undo_refuses_a_delete_through_a_parent_swapped_for_an_outside_symlink() {
    let (root, operation, path) = swapped_parent_fixture("created");
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation);
    swap_parent_for_outside_symlink(&root);
    assert_eq!(tracker.undo_last(), UndoResult::Unavailable(path));
    assert_eq!(
        fs::read_to_string(root.join("outside/note")).unwrap(),
        "outside bytes"
    );
    assert_eq!(
        fs::read_to_string(root.join("workspace/saved-sub/note")).unwrap(),
        "created"
    );
}

#[test]
fn undo_refuses_a_parent_or_anchor_replaced_by_another_directory() {
    let (root, mut operation, path) = swapped_parent_fixture("changed");
    operation.previous_content = Some(b"original".to_vec());
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation.clone());
    fs::rename(root.join("workspace/sub"), root.join("workspace/saved-sub")).unwrap();
    fs::create_dir(root.join("workspace/sub")).unwrap();
    fs::write(&path, "replacement").unwrap();
    assert_eq!(tracker.undo_last(), UndoResult::Unavailable(path.clone()));
    assert_eq!(fs::read_to_string(&path).unwrap(), "replacement");
    fs::rename(root.join("workspace"), root.join("saved-workspace")).unwrap();
    symlink(root.join("saved-workspace"), root.join("workspace")).unwrap();
    tracker.push_operation(operation);
    assert_eq!(tracker.undo_last(), UndoResult::Unavailable(path));
    assert_eq!(
        fs::read_to_string(root.join("saved-workspace/saved-sub/note")).unwrap(),
        "changed"
    );
}

#[test]
fn undo_refuses_an_operation_whose_traversal_does_not_match_its_components() {
    let root = Root::new();
    fs::create_dir(root.join("sub")).unwrap();
    let path = root.join("sub/note");
    fs::write(&path, "changed").unwrap();
    let mut operation = root.capture("sub/note", Some("original"));
    operation.parent_identities.clear();
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation);
    assert_eq!(tracker.undo_last(), UndoResult::Unavailable(path.clone()));
    assert_eq!(fs::read_to_string(&path).unwrap(), "changed");
}
