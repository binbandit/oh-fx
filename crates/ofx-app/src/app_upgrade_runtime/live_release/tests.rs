use std::fs;

use ofx_upgrade::UpgradeLock;

use super::*;

fn release_in(directory: &tempfile::TempDir) -> LiveRelease {
    let executable = directory.path().join("oh-fx");
    fs::write(&executable, b"old").unwrap();
    LiveRelease::at(directory.path().join("state"), executable).unwrap()
}

fn checked(release: &mut LiveRelease) -> (CheckOutcome, Vec<String>) {
    let control = UpgradeControl::new();
    let mut seen = Vec::new();
    let mut found = |version: &str| seen.push(version.to_owned());
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(release.check(&control, &mut found));
    (outcome, seen)
}

#[test]
fn a_binary_another_process_installed_is_ready_without_a_download() {
    let directory = tempfile::tempdir().unwrap();
    let mut release = release_in(&directory);
    let replacement = directory.path().join("oh-fx.new");
    fs::write(&replacement, b"new").unwrap();
    fs::rename(&replacement, directory.path().join("oh-fx")).unwrap();
    assert_eq!(checked(&mut release), (CheckOutcome::Installed, Vec::new()));
}

#[test]
fn an_upgrade_already_under_way_elsewhere_waits_for_the_next_check() {
    let directory = tempfile::tempdir().unwrap();
    let mut release = release_in(&directory);
    let _held = UpgradeLock::acquire(&directory.path().join("state")).unwrap();
    assert_eq!(checked(&mut release), (CheckOutcome::Current, Vec::new()));
}

#[test]
fn a_stop_before_the_check_ends_it() {
    let directory = tempfile::tempdir().unwrap();
    let mut release = release_in(&directory);
    let control = UpgradeControl::new();
    control.request_stop();
    let mut found = |_: &str| {};
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(release.check(&control, &mut found));
    assert_eq!(outcome, CheckOutcome::Stopped);
}
