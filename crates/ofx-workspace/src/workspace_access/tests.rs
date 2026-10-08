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

fn saved_paths(access: &WorkspaceAccess) -> Vec<&Path> {
    access.saved_directories().collect()
}

#[test]
fn workspace_access_stages_add_remove_clear_without_mutating_the_source() {
    let fixture = Fixture::new();
    let saved = fixture.directory("saved");
    let launched = fixture.directory("launch");
    let original = WorkspaceAccess::new(&fixture.primary, &[text(&saved)])
        .unwrap()
        .apply_launch(&launch(&[&text(&launched)]), false)
        .unwrap();
    let added = original.stage_add_saved(&text(&launched)).unwrap();
    assert_eq!(
        original.entries(),
        [
            entry(&saved, true, false, true),
            entry(&launched, false, true, true)
        ]
    );
    assert_eq!(
        added.entries(),
        [
            entry(&saved, true, false, true),
            entry(&launched, true, true, true)
        ]
    );
    assert_eq!(added.saved_sources().len(), 2);
    assert_eq!(saved_paths(&added), [saved.as_path(), launched.as_path()]);
    let removed = added.stage_remove(&text(&launched)).unwrap();
    assert_eq!(removed.entries(), [entry(&saved, true, false, true)]);
    assert_eq!(removed.saved_sources().len(), 1);
    let cleared = removed.stage_clear();
    assert!(cleared.entries().is_empty());
    assert!(cleared.saved_sources().is_empty());
}

#[test]
fn staging_an_add_resolves_the_input_once_and_keeps_saved_directories_single() {
    let fixture = Fixture::new();
    let shared = fixture.directory("shared");
    let empty = WorkspaceAccess::primary_only(&fixture.primary);
    assert_eq!(
        empty.add_directory_identity("../shared"),
        Ok(shared.clone())
    );
    let added = empty.stage_add_saved("../shared").unwrap();
    assert_eq!(added.entries(), [entry(&shared, true, false, true)]);
    assert_eq!(added.stage_add_saved("../shared/.").unwrap(), added);
    assert_eq!(
        empty.stage_add_saved("../missing"),
        Err(WorkspaceAccessError::PathNotFound)
    );
    let roots: Vec<String> = (0..MAX_ADDITIONAL_DIRECTORIES)
        .map(|index| text(&fixture.directory(&format!("root-{index}"))))
        .collect();
    assert_eq!(
        WorkspaceAccess::new(&fixture.primary, &roots)
            .unwrap()
            .stage_add_saved("../shared"),
        Err(WorkspaceAccessError::TooManyDirectories)
    );
}

#[test]
fn workspace_access_rejects_removal_of_unknown_directories() {
    let fixture = Fixture::new();
    let saved = fixture.directory("saved");
    let unknown = fixture.directory("unknown");
    let access = WorkspaceAccess::new(&fixture.primary, &[text(&saved)]).unwrap();
    for input in [text(&unknown), text(&fixture.root.join("missing"))] {
        assert_eq!(
            access.stage_remove(&input),
            Err(WorkspaceAccessError::UnknownAdditionalDirectory),
            "{input}"
        );
    }
    assert_eq!(
        access.stage_remove("../unknown"),
        Err(WorkspaceAccessError::InvalidPath)
    );
}

#[test]
fn workspace_access_removes_a_saved_directory_by_any_spelling_after_it_disappears() {
    let fixture = Fixture::new();
    let saved = fixture.directory("saved");
    let access = WorkspaceAccess::new(&fixture.primary, &[text(&saved)]).unwrap();
    fs::remove_dir(&saved).unwrap();
    for input in [
        text(&saved),
        format!("{}/", text(&saved)),
        format!("{}/.", text(&saved)),
        format!("{}/missing/..", text(&saved)),
        "../saved".to_owned(),
    ] {
        assert!(
            access.stage_remove(&input).unwrap().entries().is_empty(),
            "{input}"
        );
    }
    fs::write(&saved, "").unwrap();
    assert!(
        access
            .stage_remove(&text(&saved))
            .unwrap()
            .entries()
            .is_empty()
    );
}

#[test]
fn workspace_access_keeps_the_observed_identity_of_a_retargeted_saved_source() {
    let fixture = Fixture::new();
    let first = fixture.directory("first");
    let second = fixture.directory("second");
    let link = fixture.root.join("saved-link");
    symlink(&first, &link).unwrap();
    let access = WorkspaceAccess::new(&fixture.primary, &[text(&link)]).unwrap();
    assert_eq!(
        access.saved_sources(),
        [SavedSource {
            source: text(&link),
            identity: first.clone(),
        }]
    );
    fs::remove_file(&link).unwrap();
    symlink(&second, &link).unwrap();
    let launched = access.apply_launch(&[], false).unwrap();
    assert_eq!(launched.entries(), [entry(&first, true, false, true)]);
    assert_eq!(launched.saved_sources(), access.saved_sources());
    let removed = launched.stage_remove(&text(&link)).unwrap();
    assert!(removed.entries().is_empty());
    assert!(removed.saved_sources().is_empty());
}

#[test]
fn workspace_access_reports_when_a_staged_replacement_removes_command_line_authority() {
    let fixture = Fixture::new();
    let saved = fixture.directory("saved");
    let launched = fixture.directory("launch");
    let saved_only = WorkspaceAccess::new(&fixture.primary, &[text(&saved)]).unwrap();
    assert!(
        !saved_only.command_line_source_removed(&saved_only.stage_remove(&text(&saved)).unwrap())
    );
    let flag_only = WorkspaceAccess::primary_only(&fixture.primary)
        .apply_launch(&launch(&[&text(&launched)]), false)
        .unwrap();
    assert!(
        flag_only.command_line_source_removed(&flag_only.stage_remove(&text(&launched)).unwrap())
    );
    let both = WorkspaceAccess::new(&fixture.primary, &[text(&launched)])
        .unwrap()
        .apply_launch(&launch(&[&text(&launched)]), false)
        .unwrap();
    assert!(both.command_line_source_removed(&both.stage_remove(&text(&launched)).unwrap()));
    assert!(
        !flag_only.command_line_source_removed(&flag_only.stage_add_saved(&text(&saved)).unwrap())
    );
    assert!(flag_only.command_line_source_removed(&flag_only.stage_clear()));
}

#[test]
fn clearing_keeps_whether_saved_directories_are_suppressed() {
    let fixture = Fixture::new();
    let saved = fixture.directory("saved");
    let access = WorkspaceAccess::new(&fixture.primary, &[text(&saved)])
        .unwrap()
        .apply_launch(&[], true)
        .unwrap();
    assert!(access.saved_suppressed());
    assert!(access.stage_clear().saved_suppressed());
    assert!(!WorkspaceAccess::primary_only(&fixture.primary).saved_suppressed());
}
