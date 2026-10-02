use std::fs::{self, Permissions};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use super::*;

fn operation(path: &Path, previous_content: Option<&str>) -> FileOperation {
    FileOperation {
        path: path.to_owned(),
        previous_content: previous_content.map(|content| content.as_bytes().to_vec()),
    }
}

fn tracked(tracker: &ChangeTracker) -> Vec<PathBuf> {
    tracker
        .stack()
        .iter()
        .map(|operation| operation.path.clone())
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
    tracker.push_operation(operation(Path::new("/workspace/a.txt"), Some("before")));
    tracker.clear();
    assert!(tracked(&tracker).is_empty());
    assert_eq!(tracker.undo_last(), UndoResult::Empty);
    tracker.push_operation(operation(Path::new("/workspace/reused.txt"), None));
    assert_eq!(tracked(&tracker), [PathBuf::from("/workspace/reused.txt")]);
}

#[test]
fn push_operation_evicts_the_oldest_operation_with_stable_ordering() {
    let tracker = ChangeTracker::default();
    for index in 0..=MAX_STACK_SIZE {
        tracker.push_operation(operation(
            Path::new(&format!("/tracked/file-{index}")),
            None,
        ));
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
        .push_operation(operation(Path::new("/workspace/shared.txt"), None));
    assert_eq!(tracked(&tracker), [PathBuf::from("/workspace/shared.txt")]);
}

#[test]
fn undo_last_restores_previous_content_for_write_and_edit_operations() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("restore.txt");
    fs::write(&path, "modified").unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation(&path, Some("original")));
    assert_eq!(tracker.undo_last(), UndoResult::Restored(path.clone()));
    assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn undo_last_restores_the_newest_operation_first() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("twice.txt");
    fs::write(&path, "third").unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation(&path, Some("first")));
    tracker.push_operation(operation(&path, Some("second")));
    assert_eq!(tracker.undo_last(), UndoResult::Restored(path.clone()));
    assert_eq!(fs::read_to_string(&path).unwrap(), "second");
    assert_eq!(tracker.undo_last(), UndoResult::Restored(path.clone()));
    assert_eq!(fs::read_to_string(&path).unwrap(), "first");
    assert_eq!(tracker.undo_last(), UndoResult::Empty);
}

#[test]
fn a_restored_file_keeps_the_permissions_of_the_file_it_replaces() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("script.sh");
    fs::write(&path, "changed").unwrap();
    fs::set_permissions(&path, Permissions::from_mode(0o700)).unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation(&path, Some("original")));
    assert_eq!(tracker.undo_last(), UndoResult::Restored(path.clone()));
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & PERMISSION_BITS, 0o700);
}

#[test]
fn undo_last_deletes_new_write_and_edit_operations() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("new.txt");
    fs::write(&path, "new content").unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation(&path, None));
    assert_eq!(tracker.undo_last(), UndoResult::Deleted(path.clone()));
    assert!(!path.exists());
}

#[test]
fn undo_last_reports_deleted_for_a_new_write_when_the_file_is_already_absent() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("already-absent.txt");
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation(&path, None));
    assert_eq!(tracker.undo_last(), UndoResult::Deleted(path.clone()));
    assert!(!path.exists());
}

#[test]
fn undo_last_reports_unavailable_when_a_new_file_cannot_be_deleted() {
    let directory = tempfile::tempdir().unwrap();
    let locked = directory.path().join("locked");
    fs::create_dir(&locked).unwrap();
    let path = locked.join("new.txt");
    fs::write(&path, "new content").unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation(&path, None));
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
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("now-a-directory");
    fs::create_dir(&path).unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation(&path, None));
    assert_eq!(tracker.undo_last(), UndoResult::Unavailable(path.clone()));
    assert!(path.is_dir());
}

#[test]
fn undo_last_pops_before_filesystem_restore_failures_reports_them_and_does_not_create_parents() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("missing-parent/file.txt");
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation(&path, Some("content")));
    assert_eq!(tracker.undo_last(), UndoResult::Unavailable(path.clone()));
    assert!(tracked(&tracker).is_empty());
    assert!(!path.exists());
    assert!(!directory.path().join("missing-parent").exists());
    assert_eq!(tracker.undo_last(), UndoResult::Empty);
}

#[test]
fn undo_last_leaves_a_file_without_write_permission_intact() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("read-only.txt");
    fs::write(&path, "bytes the user still has on disk").unwrap();
    fs::set_permissions(&path, Permissions::from_mode(0o444)).unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation(&path, Some("preimage bytes")));
    assert_eq!(tracker.undo_last(), UndoResult::Unavailable(path.clone()));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "bytes the user still has on disk"
    );
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn undo_refuses_a_file_whose_directory_denies_writes_and_leaves_it_intact() {
    let directory = tempfile::tempdir().unwrap();
    let locked = directory.path().join("locked");
    fs::create_dir(&locked).unwrap();
    let path = locked.join("file.txt");
    let current = "x".repeat(64 * 1024);
    fs::write(&path, &current).unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation(&path, Some("preimage bytes")));
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
    let directory = tempfile::tempdir().unwrap();
    let recorded = directory.path().join("recorded.txt");
    let redirect_target = directory.path().join("redirect-target.txt");
    fs::write(&recorded, "current bytes").unwrap();
    fs::write(&redirect_target, "must stay untouched").unwrap();
    let tracker = ChangeTracker::default();
    tracker.push_operation(operation(&recorded, Some("preimage bytes")));
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
