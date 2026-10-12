use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::*;
use crate::session_manager::test_support::{framed, install};

const SESSIONS: [(&str, &str); 7] = [
    ("listing", "nJI52ukeF2bU"),
    ("listing", "U9K-4ZzltsUh"),
    ("listing", "pgdu6JEc1UOO"),
    ("listing", "hDoX4WdPFWDK"),
    ("tool", "ia8mVXaiyAf1"),
    ("kill_tool", "NYXx_508J28b"),
    ("snapshot_small", "PsjMeU3Vl5Ld"),
];

fn root_with(scenario: &str) -> (tempfile::TempDir, PrivateDir) {
    let directory = tempfile::tempdir().unwrap();
    install(scenario, directory.path());
    let root = PrivateDir::open_existing(directory.path())
        .unwrap()
        .unwrap();
    (directory, root)
}

const CLEAN: Verified = Verified {
    damaged_at: None,
    bad_snapshots: 0,
    bad_blobs: 0,
};

fn rewrite(path: &Path, change: impl FnOnce(&mut Vec<u8>)) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    let mut bytes = fs::read(path).unwrap();
    change(&mut bytes);
    fs::write(path, bytes).unwrap();
}

#[test]
fn every_session_fx_wrote_verifies_clean() {
    for (scenario, id) in SESSIONS {
        let (_directory, root) = root_with(scenario);
        assert_eq!(verify_session(&root, id), Ok(CLEAN), "{scenario}/{id}");
    }
}

#[test]
fn a_flipped_byte_inside_the_log_is_damage_at_its_line() {
    let (directory, root) = root_with("tool");
    let log = directory.path().join("ia8mVXaiyAf1").join(LOG_NAME);
    let original = fs::read(&log).unwrap();
    let third = original
        .split_inclusive(|&byte| byte == b'\n')
        .take(2)
        .map(<[u8]>::len)
        .sum::<usize>();
    rewrite(&log, |bytes| bytes[third + 30] ^= 0x20);
    let verified = verify_session(&root, "ia8mVXaiyAf1").unwrap();
    assert_eq!(verified.damaged_at, Some(third as u64));
}

#[test]
fn a_missing_or_changed_blob_counts_against_its_session() {
    let (directory, root) = root_with("tool");
    let blobs = directory.path().join("ia8mVXaiyAf1").join(BLOBS_DIR);
    let names: Vec<_> = fs::read_dir(&blobs)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    rewrite(&names[0], |bytes| bytes.push(b'!'));
    fs::remove_file(&names[1]).unwrap();
    let verified = verify_session(&root, "ia8mVXaiyAf1").unwrap();
    assert_eq!(verified.damaged_at, None);
    assert!(verified.bad_blobs >= 2, "{verified:?}");
}

#[test]
fn a_snapshot_that_differs_from_the_fold_at_its_place_is_bad() {
    let (directory, root) = root_with("snapshot_small");
    let log = directory.path().join("PsjMeU3Vl5Ld").join(LOG_NAME);
    rewrite(&log, |bytes| {
        let lines: Vec<Vec<u8>> = bytes
            .split_inclusive(|&byte| byte == b'\n')
            .map(|line| {
                let text = String::from_utf8(line.to_vec()).unwrap();
                if !text.contains("\"kind\":\"snapshot\"") {
                    return line.to_vec();
                }
                let payload = &text[..text.rfind(",\"crc\":\"").unwrap()];
                framed(&payload.replace("\"committed\":1,", "\"committed\":2,"))
            })
            .collect();
        *bytes = lines.concat();
    });
    let verified = verify_session(&root, "PsjMeU3Vl5Ld").unwrap();
    assert_eq!((verified.damaged_at, verified.bad_snapshots), (None, 1));
}

#[test]
fn a_session_that_is_missing_or_not_an_id_is_not_found() {
    let (_directory, root) = root_with("listing");
    assert_eq!(
        verify_session(&root, "missing00000"),
        Err(VerifyError::NotFound)
    );
    assert_eq!(verify_session(&root, "../x"), Err(VerifyError::NotFound));
}
