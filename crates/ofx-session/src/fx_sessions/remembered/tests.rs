use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use ofx_config::PrivateDir;
use ofx_text::lowercase_hex;
use sha2::{Digest, Sha256};

use super::super::tests::{Home, Saved, shell_turn_without_its_call, snapshot};
use crate::fx_sessions::FxSessions;
use crate::session_error::SessionError;
use crate::session_store::{RememberedSession, SessionStore};
use crate::session_summary_codec::SessionSource;

const WORKSPACE: &str = "/work";

fn saved(id: &'static str, workspace: &'static str, modified_s: u64) -> Saved<'static> {
    Saved {
        id,
        workspace,
        title: Some(id),
        prompts: &["one"],
        modified_s,
    }
}

fn importing(home: &Home) -> SessionStore {
    home.store(WORKSPACE)
        .with_fx_home(home.path().to_path_buf())
}

fn fx_continue(home: &Home) -> PathBuf {
    home.fx_profile().join("continue")
}

fn pointer_name() -> String {
    lowercase_hex(&Sha256::digest(WORKSPACE.as_bytes()))
}

fn point_fx_at(home: &Home, bytes: &[u8]) -> PathBuf {
    let directory = fx_continue(home);
    fs::create_dir_all(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let pointer = directory.join(pointer_name());
    fs::write(&pointer, bytes).unwrap();
    fs::set_permissions(&pointer, fs::Permissions::from_mode(0o600)).unwrap();
    pointer
}

type Spoil = fn(&Home);

fn remembered(store: &SessionStore) -> Option<RememberedSession> {
    store.remembered_session().unwrap()
}

fn named(id: &str, source: SessionSource) -> RememberedSession {
    RememberedSession {
        id: id.to_owned(),
        source,
    }
}

#[test]
fn an_fx_pointer_alone_names_the_session_to_continue() {
    let home = Home::new();
    saved("fx-continued", WORKSPACE, 100).write(&home.fx_sessions());
    point_fx_at(&home, b"fx-continued\n");
    let before = snapshot(&home.fx_profile());

    assert_eq!(
        remembered(&importing(&home)),
        Some(named("fx-continued", SessionSource::Fx))
    );
    assert_eq!(remembered(&home.store(WORKSPACE)), None);
    assert_eq!(
        remembered(
            &home
                .store("/elsewhere")
                .with_fx_home(home.path().to_path_buf())
        ),
        None
    );
    let session = importing(&home).resume("fx-continued").unwrap();
    assert_eq!(session.title(), Some("fx-continued"));
    drop(session);
    assert_eq!(snapshot(&home.fx_profile()), before);
}

#[test]
fn an_oh_fx_pointer_alone_is_continued_as_before() {
    let home = Home::new();
    let store = importing(&home);
    saved("own-continued", WORKSPACE, 100).write(&home.own_sessions());
    store.remember_session_id("own-continued").unwrap();
    assert_eq!(
        remembered(&store),
        Some(named("own-continued", SessionSource::OhFx))
    );
}

#[test]
fn with_both_pointers_the_session_used_last_is_continued() {
    for (own_s, fx_s, expected) in [
        (200, 300, Some(named("fx-pointed", SessionSource::Fx))),
        (300, 200, Some(named("own-pointed", SessionSource::OhFx))),
        (200, 200, Some(named("own-pointed", SessionSource::OhFx))),
    ] {
        let home = Home::new();
        let store = importing(&home);
        saved("own-pointed", WORKSPACE, own_s).write(&home.own_sessions());
        store.remember_session_id("own-pointed").unwrap();
        saved("fx-pointed", WORKSPACE, fx_s).write(&home.fx_sessions());
        point_fx_at(&home, b"fx-pointed\n");
        assert_eq!(remembered(&store), expected, "{own_s} {fx_s}");
    }
}

#[test]
fn a_session_both_pointers_name_is_continued_as_oh_fx_remembers_it() {
    let home = Home::new();
    let store = importing(&home);
    saved("shared-id", WORKSPACE, 100).write(&home.fx_sessions());
    drop(store.resume("shared-id").unwrap());
    store.remember_session_id("shared-id").unwrap();
    point_fx_at(&home, b"shared-id\n");
    assert_eq!(
        remembered(&store),
        Some(named("shared-id", SessionSource::OhFx))
    );
}

#[test]
fn an_fx_pointer_that_cannot_be_trusted_is_ignored() {
    let cases: [(&str, Spoil); 8] = [
        ("no newline", |home| {
            point_fx_at(home, b"fx-pointed");
        }),
        ("not an id", |home| {
            point_fx_at(home, b"../fx-pointed\n");
        }),
        ("too big", |home| {
            point_fx_at(home, &[b'a'; 300]);
        }),
        ("shared", |home| {
            let pointer = point_fx_at(home, b"fx-pointed\n");
            fs::set_permissions(pointer, fs::Permissions::from_mode(0o644)).unwrap();
        }),
        ("linked", |home| {
            let pointer = point_fx_at(home, b"fx-pointed\n");
            let target = home.path().join("elsewhere-pointer");
            fs::rename(&pointer, &target).unwrap();
            symlink(&target, &pointer).unwrap();
        }),
        ("hard linked", |home| {
            let pointer = point_fx_at(home, b"fx-pointed\n");
            fs::hard_link(&pointer, home.path().join("second-name")).unwrap();
        }),
        ("open folder", |home| {
            point_fx_at(home, b"fx-pointed\n");
            fs::set_permissions(fx_continue(home), fs::Permissions::from_mode(0o755)).unwrap();
        }),
        ("open profile", |home| {
            point_fx_at(home, b"fx-pointed\n");
            fs::set_permissions(home.fx_profile(), fs::Permissions::from_mode(0o755)).unwrap();
        }),
    ];
    for (case, spoil) in cases {
        let home = Home::new();
        saved("fx-pointed", WORKSPACE, 300).write(&home.fx_sessions());
        spoil(&home);
        let store = importing(&home);
        assert_eq!(remembered(&store), None, "{case}");
        saved("own-pointed", WORKSPACE, 100).write(&home.own_sessions());
        store.remember_session_id("own-pointed").unwrap();
        assert_eq!(
            remembered(&store),
            Some(named("own-pointed", SessionSource::OhFx)),
            "{case}"
        );
    }
}

#[test]
fn an_fx_pointer_to_a_session_oh_fx_cannot_resume_loses_to_oh_fx_and_explains_itself_alone() {
    let home = Home::new();
    let orphan = saved("fx-orphan", WORKSPACE, 300);
    orphan.write_files(
        &home.fx_sessions(),
        &orphan.manifest(),
        &shell_turn_without_its_call(),
    );
    for (pointed, failure) in [
        ("fx-orphan", SessionError::FxSessionUnreadable),
        ("fx-missing", SessionError::SessionNotFound),
    ] {
        let home_store = importing(&home);
        point_fx_at(&home, format!("{pointed}\n").as_bytes());
        assert_eq!(
            remembered(&home_store),
            Some(named(pointed, SessionSource::Fx)),
            "{pointed}"
        );
        assert_eq!(home_store.resume(pointed).err(), Some(failure), "{pointed}");
    }
    let store = importing(&home);
    saved("own-pointed", WORKSPACE, 100).write(&home.own_sessions());
    store.remember_session_id("own-pointed").unwrap();
    for pointed in ["fx-orphan", "fx-missing"] {
        point_fx_at(&home, format!("{pointed}\n").as_bytes());
        assert_eq!(
            remembered(&store),
            Some(named("own-pointed", SessionSource::OhFx)),
            "{pointed}"
        );
    }
}

#[test]
fn resume_last_opens_the_newest_session_of_this_workspace_in_either_store() {
    let home = Home::new();
    saved("own-older", WORKSPACE, 200).write(&home.own_sessions());
    saved("fx-newest", WORKSPACE, 300).write(&home.fx_sessions());
    saved("fx-elsewhere", "/elsewhere", 500).write(&home.fx_sessions());
    let unreadable = saved("fx-unreadable", WORKSPACE, 400);
    unreadable.write_files(
        &home.fx_sessions(),
        &unreadable.manifest(),
        &shell_turn_without_its_call(),
    );
    let before = snapshot(&home.fx_profile());

    assert_eq!(
        home.store(WORKSPACE).resume_latest().unwrap().id(),
        "own-older"
    );
    assert!(!home.own_sessions().join("fx-newest").exists());

    let session = importing(&home).resume_latest().unwrap();
    assert_eq!(session.id(), "fx-newest");
    drop(session);
    assert!(
        home.own_sessions()
            .join("fx-newest")
            .join("fx-import.json")
            .exists()
    );
    assert!(!home.own_sessions().join("fx-elsewhere").exists());
    assert_eq!(snapshot(&home.fx_profile()), before);

    saved("own-newest", WORKSPACE, 4_102_444_800).write(&home.own_sessions());
    assert_eq!(importing(&home).resume_latest().unwrap().id(), "own-newest");
}

#[test]
fn resume_last_skips_an_fx_session_moved_to_another_workspace_after_it_was_listed() {
    let home = Home::new();
    saved("fx-moving", WORKSPACE, 300).write(&home.fx_sessions());
    let store = importing(&home);
    let catalog = store
        .catalog_with_fx(&FxSessions::open(home.path()))
        .unwrap();
    let listed = catalog
        .summaries()
        .iter()
        .find(|summary| summary.id == "fx-moving")
        .unwrap();
    assert_eq!(listed.workspace_root, WORKSPACE);
    saved("fx-moving", "/elsewhere", 300).write(&home.fx_sessions());
    let before = snapshot(&home.fx_profile());

    assert_eq!(
        store.open_latest_candidate(listed).err(),
        Some(SessionError::SessionTargetChanged)
    );
    let copy: serde_json::Value = serde_json::from_slice(
        &fs::read(home.own_sessions().join("fx-moving").join("session.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(copy["workspace_root"], "/elsewhere");
    assert_eq!(snapshot(&home.fx_profile()), before);
    assert_eq!(
        store.resume_latest().err(),
        Some(SessionError::NoSavedSessions)
    );
}

#[test]
fn resume_last_reports_an_fx_session_it_cannot_open() {
    let home = Home::new();
    saved("own-older", WORKSPACE, 200).write(&home.own_sessions());
    saved("fx-newest", WORKSPACE, 300).write(&home.fx_sessions());
    let source = PrivateDir::open_existing(&home.fx_sessions().join("fx-newest"))
        .unwrap()
        .unwrap();
    let held = source.try_lock("session.lock").unwrap().unwrap();
    assert_eq!(
        importing(&home).resume_latest().err(),
        Some(SessionError::FxSessionOpen)
    );
    drop(held);
}
