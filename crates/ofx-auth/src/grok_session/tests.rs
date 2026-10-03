use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

use super::*;
use crate::session_presence::Presence;
use crate::subscription_session::DeleteOutcome;

fn session() -> Session {
    Session {
        access_token: Secret::new("header.payload.signature".to_owned()),
        refresh_token: Secret::new("refresh".to_owned()),
        expires_at_ms: 1234,
        account_id: "acct_123".to_owned(),
    }
}

fn store(root: &tempfile::TempDir) -> SessionStore {
    SessionStore::new(root.path().join("oh-fx"))
}

#[test]
fn grok_auth_session_round_trips_with_exact_schema_and_redacted_secrets() {
    let session = session();
    let encoded = stringify(&session).unwrap();
    assert_eq!(
        encoded.as_str(),
        "{\"version\":1,\"access_token\":\"header.payload.signature\",\"refresh_token\":\"refresh\",\"expires_at_ms\":1234,\"account_id\":\"acct_123\"}\n"
    );
    assert_eq!(parse(encoded.as_bytes()).unwrap(), session);
    let debug = format!("{session:?}");
    assert!(!debug.contains("header.payload.signature"));
    assert!(!debug.contains("\"refresh\""));
}

#[test]
fn account_identity_is_bounded_and_safe_for_http_headers() {
    for account in [
        String::new(),
        "acct\r\ninjected".to_owned(),
        "space here".to_owned(),
        "a".repeat(1025),
        "é".to_owned(),
    ] {
        let mut invalid = session();
        invalid.account_id = account.clone();
        assert_eq!(stringify(&invalid), Err(GrokError::InvalidGrokAuthSession));
        let encoded = serde_json::json!({"version":1,"access_token":"a","refresh_token":"r",
            "expires_at_ms":1,"account_id":account})
        .to_string();
        assert_eq!(
            parse(encoded.as_bytes()),
            Err(GrokError::InvalidGrokAuthSession)
        );
    }
    let mut boundary = session();
    boundary.account_id = "a".repeat(1024);
    assert_eq!(
        parse(stringify(&boundary).unwrap().as_bytes()).unwrap(),
        boundary
    );
}

#[test]
fn session_refresh_deadline_keeps_a_one_minute_safety_margin() {
    assert_eq!(refresh_deadline_ms(100_000), 40_000);
    assert_eq!(refresh_deadline_ms(10_000), 0);
    assert_eq!(refresh_deadline_ms(i64::MIN), 0);
    let mut session = session();
    session.expires_at_ms = 100_000;
    assert!(!session.expired(39_999));
    assert!(session.expired(40_000));
}

#[test]
fn session_schema_requires_integer_version_expiry_and_nonempty_strings() {
    let valid = serde_json::json!({"version":1,"access_token":"a","refresh_token":"r",
        "expires_at_ms":1,"account_id":"x"});
    for (field, value) in [
        ("version", serde_json::json!(2)),
        ("version", serde_json::json!(1.0)),
        ("access_token", serde_json::json!("")),
        ("refresh_token", serde_json::json!(null)),
        ("expires_at_ms", serde_json::json!("1")),
        ("expires_at_ms", serde_json::json!(1.0)),
        (
            "expires_at_ms",
            serde_json::json!(9_223_372_036_854_775_808_u64),
        ),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        assert_eq!(
            parse(invalid.to_string().as_bytes()),
            Err(GrokError::InvalidGrokAuthSession)
        );
    }
    for bytes in [
        b"[]".as_slice(),
        b"null",
        b"{",
        b"{\"version\":1,\"version\":1}",
    ] {
        assert_eq!(parse(bytes), Err(GrokError::InvalidGrokAuthSession));
    }
    for field in [
        "version",
        "access_token",
        "refresh_token",
        "expires_at_ms",
        "account_id",
    ] {
        let mut invalid = valid.clone();
        invalid.as_object_mut().unwrap().remove(field);
        assert_eq!(
            parse(invalid.to_string().as_bytes()),
            Err(GrokError::InvalidGrokAuthSession)
        );
    }
    let mut negative = session();
    negative.expires_at_ms = -1;
    assert_eq!(
        parse(stringify(&negative).unwrap().as_bytes()).unwrap(),
        negative
    );
}

#[tokio::test]
async fn missing_stores_are_admissible_without_creating_auth_files() {
    let root = tempfile::tempdir().unwrap();
    let store = store(&root);
    assert_eq!(store.presence(), Presence::Missing);
    assert_eq!(store.require_sign_in_storage(), Ok(()));
    assert!(store.begin_existing_mutation().await.unwrap().is_none());
    assert!(!root.path().join("oh-fx").exists());
}

#[tokio::test]
async fn saved_sessions_are_private_and_mutations_round_trip_and_delete() {
    let root = tempfile::tempdir().unwrap();
    let store = store(&root);
    store.save_new_session(&session()).await.unwrap();
    let path = root.path().join("oh-fx");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for name in [AUTH_FILE_NAME, MUTATION_LOCK_FILE_NAME] {
        assert_eq!(
            fs::metadata(path.join(name)).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(store.presence(), Presence::Present);
    let mutation = store.begin_existing_mutation().await.unwrap().unwrap();
    assert_eq!(mutation.require_writable(), Ok(()));
    assert_eq!(mutation.load().unwrap(), Some(session()));
    assert_eq!(mutation.delete(), Ok(DeleteOutcome::Deleted));
    assert_eq!(mutation.delete(), Ok(DeleteOutcome::Missing));
    assert_eq!(mutation.load(), Ok(None));
}

#[tokio::test]
async fn malformed_private_files_are_present_but_not_valid_sessions() {
    let root = tempfile::tempdir().unwrap();
    let store = store(&root);
    let directory = PrivateDir::open_or_create(&root.path().join("oh-fx")).unwrap();
    directory
        .replace(AUTH_FILE_NAME, b"not valid session JSON")
        .unwrap();
    assert_eq!(store.presence(), Presence::Present);
    assert_eq!(store.require_sign_in_storage(), Ok(()));
    let mutation = store.begin_existing_mutation().await.unwrap().unwrap();
    assert_eq!(mutation.load(), Err(GrokError::InvalidGrokAuthSession));
}

#[tokio::test]
async fn auth_file_size_limits_and_empty_presence_are_enforced() {
    let root = tempfile::tempdir().unwrap();
    let store = store(&root);
    let directory = PrivateDir::open_or_create(&root.path().join("oh-fx")).unwrap();
    directory.replace(AUTH_FILE_NAME, b"").unwrap();
    assert_eq!(store.presence(), Presence::Unavailable);
    directory
        .replace(AUTH_FILE_NAME, &vec![b' '; MAX_AUTH_FILE_BYTES])
        .unwrap();
    assert_eq!(store.presence(), Presence::Present);
    {
        let mutation = store.begin_existing_mutation().await.unwrap().unwrap();
        assert_eq!(mutation.load(), Err(GrokError::InvalidGrokAuthSession));
    }
    directory
        .replace(AUTH_FILE_NAME, &vec![b' '; MAX_AUTH_FILE_BYTES + 1])
        .unwrap();
    assert_eq!(store.presence(), Presence::Unavailable);
    let mutation = store.begin_existing_mutation().await.unwrap().unwrap();
    assert_eq!(
        mutation.load(),
        Err(GrokError::CredentialStorageUnavailable)
    );
}

#[tokio::test]
async fn shared_auth_files_are_insecure_and_cannot_admit_writes() {
    let root = tempfile::tempdir().unwrap();
    let store = store(&root);
    store.save_new_session(&session()).await.unwrap();
    let path = root.path().join("oh-fx").join(AUTH_FILE_NAME);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(store.presence(), Presence::Unavailable);
    assert_eq!(
        store.require_sign_in_storage(),
        Err(GrokError::CredentialStorageUnavailable)
    );
    let mutation = store.begin_existing_mutation().await.unwrap().unwrap();
    assert_eq!(mutation.load(), Err(GrokError::InsecureAuthFile));
    assert_eq!(
        mutation.require_writable(),
        Err(GrokError::CredentialStorageUnavailable)
    );
}

#[tokio::test]
async fn symlinked_and_hardlinked_auth_files_are_never_loaded() {
    for hard_link in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let store = store(&root);
        let directory = PrivateDir::open_or_create(&root.path().join("oh-fx")).unwrap();
        directory
            .replace("other.json", stringify(&session()).unwrap().as_bytes())
            .unwrap();
        let target = root.path().join("oh-fx/other.json");
        let auth = root.path().join("oh-fx").join(AUTH_FILE_NAME);
        if hard_link {
            fs::hard_link(&target, &auth).unwrap();
        } else {
            symlink(&target, &auth).unwrap();
        }
        assert_eq!(store.presence(), Presence::Unavailable);
        assert_eq!(
            store.require_sign_in_storage(),
            Err(GrokError::CredentialStorageUnavailable)
        );
        let mutation = store.begin_existing_mutation().await.unwrap().unwrap();
        assert_eq!(
            mutation.load(),
            Err(if hard_link {
                GrokError::InsecureAuthFile
            } else {
                GrokError::CredentialStorageUnavailable
            })
        );
        assert_eq!(mutation.save(&session()), Err(GrokError::DurablePathUnsafe));
    }
}

#[tokio::test]
async fn symlinked_profile_directories_are_unavailable() {
    let root = tempfile::tempdir().unwrap();
    let other = root.path().join("elsewhere");
    fs::create_dir(&other).unwrap();
    symlink(&other, root.path().join("oh-fx")).unwrap();
    let store = store(&root);
    assert_eq!(store.presence(), Presence::Unavailable);
    assert_eq!(
        store.begin_existing_mutation().await.unwrap_err(),
        GrokError::CredentialStorageUnavailable
    );
    assert_eq!(
        store.save_new_session(&session()).await,
        Err(GrokError::DurablePathUnsafe)
    );
    assert!(!other.join(AUTH_FILE_NAME).exists());
}

#[tokio::test]
async fn mutation_lock_waits_then_reports_busy_without_rewriting_credentials() {
    let root = tempfile::tempdir().unwrap();
    let store = store(&root);
    store.save_new_session(&session()).await.unwrap();
    let holder = PrivateDir::open_or_create(&root.path().join("oh-fx")).unwrap();
    let held = holder.try_lock(MUTATION_LOCK_FILE_NAME).unwrap().unwrap();
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(held);
    });
    store.begin_existing_mutation().await.unwrap().unwrap();
    release.await.unwrap();
    let held = holder.try_lock(MUTATION_LOCK_FILE_NAME).unwrap().unwrap();
    let started = Instant::now();
    assert_eq!(
        store.begin_existing_mutation().await.unwrap_err(),
        GrokError::LockBusy
    );
    assert!(started.elapsed() >= MUTATION_LOCK_WAIT);
    drop(held);
    let mutation = store.begin_existing_mutation().await.unwrap().unwrap();
    assert_eq!(mutation.load().unwrap(), Some(session()));
}

#[tokio::test]
async fn sign_in_admission_checks_the_lock_file_and_profile_write_permissions() {
    let root = tempfile::tempdir().unwrap();
    let store = store(&root);
    store.save_new_session(&session()).await.unwrap();
    let path = root.path().join("oh-fx");
    let lock = path.join(MUTATION_LOCK_FILE_NAME);
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        store.require_sign_in_storage(),
        Err(GrokError::CredentialStorageUnavailable)
    );
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
    let admitted = store.require_sign_in_storage();
    let mutation = store.begin_existing_mutation().await;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(admitted, Err(GrokError::CredentialStorageUnavailable));
    assert_eq!(
        mutation.unwrap_err(),
        GrokError::CredentialStorageUnavailable
    );
}

#[tokio::test]
async fn stored_load_never_takes_the_mutation_lock_or_changes_permissions() {
    let root = tempfile::tempdir().unwrap();
    let store = store(&root);
    assert_eq!(store.load(), Ok(None));
    store.save_new_session(&session()).await.unwrap();
    let path = root.path().join("oh-fx");
    let holder = PrivateDir::open_or_create(&path).unwrap();
    let held = holder.try_lock(MUTATION_LOCK_FILE_NAME).unwrap().unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
    let loaded = store.load();
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(loaded, Ok(Some(session())));
    assert_eq!(mode, 0o500);
    drop(held);
}
