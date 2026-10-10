use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::*;

const ID: &str = "legacy-session";
const MARKER: &str = "{\"schema_version\":1,\"session_id\":\"legacy-session\",\"authority_id\":\"03030303030303030303030303030303\",\"storage_format\":\"event_log_v1\",\"source\":\"native_create\"}\n";

fn session_dir(files: &[(&str, &str)]) -> (tempfile::TempDir, PrivateDir) {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    for (name, bytes) in files {
        let path = root.path().join(name);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let dir = PrivateDir::open_existing(root.path()).unwrap().unwrap();
    (root, dir)
}

#[test]
fn the_authority_marker_upstream_writes_names_a_schema_v3_session() {
    let (_root, dir) = session_dir(&[("authority.json", MARKER)]);
    assert!(holds_authority_marker(&dir).unwrap());
    require_schema_v3(&dir, ID).unwrap();
    let migrated = MARKER.replace("native_create", "legacy_migration");
    let (_root, dir) = session_dir(&[("authority.json", &migrated)]);
    require_schema_v3(&dir, ID).unwrap();
}

#[test]
fn a_marker_for_another_session_or_format_is_refused() {
    for marker in [
        MARKER.replace("legacy-session", "other-session"),
        MARKER.replace("event_log_v1", "event_log_v2"),
        MARKER.replace("\"schema_version\":1", "\"schema_version\":2"),
        MARKER.replace("native_create", "imported"),
        MARKER.replace("0303", "03ZZ"),
        MARKER.replace("03030303030303030303030303030303", "0303"),
        "[]".to_owned(),
        "not json".to_owned(),
    ] {
        let (_root, dir) = session_dir(&[("authority.json", &marker)]);
        assert_eq!(
            require_schema_v3(&dir, ID),
            Err(SessionError::InvalidSessionFormat),
            "{marker}"
        );
    }
}

#[test]
fn an_upgrade_fence_or_a_missing_marker_is_not_read_as_schema_v3() {
    let (_root, dir) = session_dir(&[("authority.json", MARKER), ("authority.pending.json", "{}")]);
    assert_eq!(
        require_schema_v3(&dir, ID),
        Err(SessionError::InvalidSessionFormat)
    );
    let (_root, dir) = session_dir(&[("session.json", "{}")]);
    assert!(!holds_authority_marker(&dir).unwrap());
    assert_eq!(
        require_schema_v3(&dir, ID),
        Err(SessionError::InvalidSessionFormat)
    );
}

#[test]
fn identifiers_are_canonical_lowercase_hex() {
    assert_eq!(
        parse_identifier("0102030405060708090a0b0c0d0e0f10"),
        Some([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
    );
    for invalid in [
        "0102030405060708090A0B0C0D0E0F10",
        "0102030405060708090a0b0c0d0e0f1",
        "0102030405060708090a0b0c0d0e0f1000",
        "+102030405060708090a0b0c0d0e0f10",
    ] {
        assert_eq!(parse_identifier(invalid), None, "{invalid}");
    }
}
