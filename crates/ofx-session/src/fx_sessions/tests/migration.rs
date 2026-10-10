use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::{Home, Saved, snapshot};
use crate::session_error::SessionError;
use crate::session_migration::tests::{LegacyLog, reply};
use crate::session_store::SessionStore;
use crate::session_store_types::{SessionMigration, SessionMigrationStatus};

const WORKSPACE: &str = "/work";

fn saved(id: &str) -> Saved<'_> {
    Saved {
        id,
        workspace: WORKSPACE,
        title: None,
        prompts: &["one"],
        modified_s: 100,
    }
}

fn store(home: &Home) -> SessionStore {
    home.store(WORKSPACE)
        .with_fx_home(home.path().to_path_buf())
}

fn current(id: &str) -> SessionMigration {
    SessionMigration {
        session_id: id.to_owned(),
        source_schema_version: 4,
        source_bytes: 0,
        status: SessionMigrationStatus::AlreadyCurrent,
    }
}

fn folder(home: &Home, id: &str, manifest: Option<&str>) {
    let dir = home.own_sessions().join(id);
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    if let Some(manifest) = manifest {
        fs::write(dir.join("session.json"), manifest).unwrap();
        fs::set_permissions(dir.join("session.json"), fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[test]
fn a_current_session_in_either_store_is_already_current_and_nothing_is_copied() {
    let home = Home::new();
    let store = store(&home);
    saved("own").write(&home.own_sessions());
    saved("in-fx").write(&home.fx_sessions());
    let before = snapshot(&home.fx_profile());
    assert_eq!(store.migrate("own"), Ok(current("own")));
    assert_eq!(store.migrate("in-fx"), Ok(current("in-fx")));
    assert!(!home.own_sessions().join("in-fx").exists());
    assert_eq!(snapshot(&home.fx_profile()), before);
}

#[test]
fn a_schema_v3_session_fx_saved_is_converted_into_a_copy_once() {
    let home = Home::new();
    let store = store(&home);
    let log = LegacyLog::started("fx-v3", WORKSPACE)
        .turn(&reply("first", "one"))
        .turn(&reply("second", "two"));
    log.write(&home.fx_sessions());
    let committed: u64 = log.events().len().try_into().unwrap();
    let before = snapshot(&home.fx_profile());

    assert_eq!(
        store.migrate("fx-v3"),
        Ok(SessionMigration {
            session_id: "fx-v3".to_owned(),
            source_schema_version: 3,
            source_bytes: committed,
            status: SessionMigrationStatus::Migrated,
        })
    );
    assert_eq!(store.load("fx-v3").unwrap().history.turns.len(), 2);
    assert!(home.own_sessions().join("fx-v3/fx-import.json").exists());
    assert_eq!(store.migrate("fx-v3"), Ok(current("fx-v3")));
    assert_eq!(snapshot(&home.fx_profile()), before);
}

#[test]
fn a_session_that_cannot_be_migrated_names_why() {
    let home = Home::new();
    let store = store(&home);
    folder(&home, "broken", Some("{"));
    folder(&home, "future", Some("{\"schema_version\":99}"));
    folder(&home, "empty", None);
    let fx_broken = home.fx_sessions().join("fx-broken");
    fs::create_dir_all(&fx_broken).unwrap();
    fs::set_permissions(&fx_broken, fs::Permissions::from_mode(0o700)).unwrap();
    for (id, error) in [
        ("../x", SessionError::InvalidSessionId),
        ("missing", SessionError::SessionNotFound),
        ("broken", SessionError::InvalidSessionFormat),
        ("future", SessionError::UnsupportedSessionSchema),
        ("empty", SessionError::SessionNotFound),
        ("fx-broken", SessionError::FxSessionUnreadable),
    ] {
        assert_eq!(store.migrate(id), Err(error), "{id}");
    }
    assert_eq!(
        home.store(WORKSPACE).migrate("fx-broken"),
        Err(SessionError::SessionNotFound)
    );
}
