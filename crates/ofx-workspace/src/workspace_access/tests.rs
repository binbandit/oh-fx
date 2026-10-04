use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::*;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    primary: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let primary = root.join("primary");
        fs::create_dir(&primary).unwrap();
        Self {
            _temp: temp,
            root,
            primary,
        }
    }

    fn directory(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }
}

fn text(path: &Path) -> String {
    path.to_str().unwrap().to_owned()
}

fn launch(paths: &[&str]) -> Vec<OsString> {
    paths.iter().map(OsString::from).collect()
}

fn entry(path: &Path, saved: bool, command_line: bool, available: bool) -> AdditionalDirectory {
    AdditionalDirectory {
        path: path.to_path_buf(),
        source: DirectorySource {
            saved,
            command_line,
        },
        available,
        active: available,
    }
}

#[test]
fn workspace_access_merges_source_state_without_moving_roots() {
    let fixture = Fixture::new();
    let shared = fixture.directory("shared");
    let docs = fixture.directory("docs");
    let access = WorkspaceAccess::new(&fixture.primary, &[text(&shared), text(&docs)])
        .unwrap()
        .apply_launch(&launch(&[&text(&shared)]), false)
        .unwrap();
    assert_eq!(
        access.entries(),
        [
            entry(&shared, true, true, true),
            entry(&docs, true, false, true)
        ]
    );
}

#[test]
fn launch_directories_resolve_from_the_primary_workspace_through_symlinks_in_argument_order() {
    let fixture = Fixture::new();
    let shared = fixture.directory("shared");
    let nested = fixture.directory("primary/nested");
    symlink(&shared, fixture.root.join("link")).unwrap();
    let access = WorkspaceAccess::new(&fixture.primary, &[])
        .unwrap()
        .apply_launch(&launch(&["nested", "../link", &text(&shared)]), false)
        .unwrap();
    assert_eq!(
        access.entries(),
        [
            entry(&nested, false, true, true),
            entry(&shared, false, true, true)
        ]
    );
}

#[test]
fn workspace_access_retains_missing_saved_directories_as_inactive() {
    let fixture = Fixture::new();
    let missing = fixture.root.join("missing");
    let access = WorkspaceAccess::new(&fixture.primary, &[text(&missing)]).unwrap();
    assert_eq!(access.entries(), [entry(&missing, true, false, false)]);
    assert_eq!(access.active_roots().count(), 0);
}

#[test]
fn workspace_access_normalizes_equivalent_missing_saved_directory_identities() {
    let fixture = Fixture::new();
    let missing = fixture.root.join("missing");
    let access = WorkspaceAccess::new(
        &fixture.primary,
        &[
            format!("{}/.", text(&missing)),
            format!("{}/child/..", text(&missing)),
        ],
    )
    .unwrap();
    assert_eq!(access.entries(), [entry(&missing, true, false, false)]);
}

#[test]
fn workspace_access_resolves_unavailable_identities_through_existing_ancestor_links() {
    let fixture = Fixture::new();
    let real_parent = fixture.directory("real-parent");
    symlink(&real_parent, fixture.root.join("parent-link")).unwrap();
    let real_missing = real_parent.join("missing");
    let access = WorkspaceAccess::new(
        &fixture.primary,
        &[
            text(&real_missing),
            text(&fixture.root.join("parent-link/missing")),
        ],
    )
    .unwrap();
    assert_eq!(access.entries(), [entry(&real_missing, true, false, false)]);
}

#[test]
fn workspace_access_suppresses_saved_roots_but_keeps_command_line_roots() {
    let fixture = Fixture::new();
    let saved = fixture.directory("saved");
    let launched = fixture.directory("launch");
    let access = WorkspaceAccess::new(&fixture.primary, &[text(&saved)])
        .unwrap()
        .apply_launch(&launch(&[&text(&launched)]), true)
        .unwrap();
    assert_eq!(
        access.entries(),
        [
            AdditionalDirectory {
                active: false,
                ..entry(&saved, true, false, true)
            },
            entry(&launched, false, true, true)
        ]
    );
    assert_eq!(access.active_roots().collect::<Vec<_>>(), [launched]);
}

#[test]
fn a_later_launch_replaces_the_command_line_roots_and_keeps_the_saved_ones() {
    let fixture = Fixture::new();
    let saved = fixture.directory("saved");
    let first = fixture.directory("first");
    let second = fixture.directory("second");
    let access = WorkspaceAccess::new(&fixture.primary, &[text(&saved)])
        .unwrap()
        .apply_launch(&launch(&[&text(&first), &text(&saved)]), false)
        .unwrap()
        .apply_launch(&launch(&[&text(&second)]), false)
        .unwrap();
    assert_eq!(
        access.entries(),
        [
            entry(&saved, true, false, true),
            entry(&second, false, true, true)
        ]
    );
}

#[test]
fn workspace_access_rejects_the_primary_root_and_capacity_overflow() {
    let fixture = Fixture::new();
    assert_eq!(
        WorkspaceAccess::new(&fixture.primary, &[text(&fixture.primary)]),
        Err(WorkspaceAccessError::PrimaryDirectory)
    );
    assert_eq!(
        WorkspaceAccess::new(&fixture.primary, &[])
            .unwrap()
            .apply_launch(&launch(&["."]), false),
        Err(WorkspaceAccessError::PrimaryDirectory)
    );
    let roots: Vec<String> = (0..=MAX_ADDITIONAL_DIRECTORIES)
        .map(|index| text(&fixture.directory(&format!("root-{index}"))))
        .collect();
    assert_eq!(
        WorkspaceAccess::new(&fixture.primary, &roots),
        Err(WorkspaceAccessError::TooManyDirectories)
    );
    let saved = &roots[..MAX_ADDITIONAL_DIRECTORIES];
    assert_eq!(
        WorkspaceAccess::new(&fixture.primary, saved)
            .unwrap()
            .apply_launch(&launch(&[&roots[MAX_ADDITIONAL_DIRECTORIES]]), true),
        Err(WorkspaceAccessError::TooManyDirectories)
    );
}

#[test]
fn launch_directories_that_cannot_be_used_fail_with_upstream_error_names() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("file"), "").unwrap();
    let cases = [
        (OsString::from(""), WorkspaceAccessError::InvalidPath),
        (OsString::from("a\0b"), WorkspaceAccessError::InvalidPath),
        (
            OsString::from_vec(vec![0xff]),
            WorkspaceAccessError::InvalidPath,
        ),
        (
            OsString::from("x".repeat(MAX_PATH_BYTES + 1)),
            WorkspaceAccessError::InvalidPath,
        ),
        (
            OsString::from("../absent"),
            WorkspaceAccessError::PathNotFound,
        ),
        (
            OsString::from("../file"),
            WorkspaceAccessError::NotDirectory,
        ),
        (
            OsString::from("../file/below"),
            WorkspaceAccessError::NotDirectory,
        ),
    ];
    for (path, error) in cases {
        assert_eq!(
            WorkspaceAccess::new(&fixture.primary, &[])
                .unwrap()
                .apply_launch(std::slice::from_ref(&path), false),
            Err(error),
            "{path:?}"
        );
    }
    assert_eq!(
        WorkspaceAccessError::PathNotFound.to_string(),
        "PathNotFound"
    );
}

#[test]
fn saved_directories_must_be_absolute() {
    let fixture = Fixture::new();
    fixture.directory("relative");
    assert_eq!(
        WorkspaceAccess::new(&fixture.primary, &["../relative".to_owned()]),
        Err(WorkspaceAccessError::InvalidPath)
    );
}

#[test]
fn only_active_roots_hold_paths_beyond_the_primary_workspace() {
    let fixture = Fixture::new();
    let shared = fixture.directory("shared");
    let offline = fixture.root.join("offline");
    let access = WorkspaceAccess::new(&fixture.primary, &[text(&shared), text(&offline)]).unwrap();
    assert_eq!(
        access.additional_root_for(&shared.join("lib.rs")),
        Some(shared.as_path())
    );
    assert_eq!(access.additional_root_for(&offline.join("file.txt")), None);
    assert_eq!(
        access.additional_root_for(&fixture.root.join("external/file.txt")),
        None
    );
    assert_eq!(
        access.additional_root_for(&fixture.primary.join("src/main.rs")),
        None
    );
}
