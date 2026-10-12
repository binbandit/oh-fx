use std::fs;
use std::os::unix::fs::PermissionsExt;

use ofx_config::PrivateDir;

use super::*;
use crate::session_manager::schema::SetKey;
use crate::session_manager::test_support::{fixture_log, install, line};

fn root_with(scenario: &str) -> (tempfile::TempDir, PrivateDir) {
    let directory = tempfile::tempdir().unwrap();
    install(scenario, directory.path());
    let root = PrivateDir::open_existing(directory.path())
        .unwrap()
        .unwrap();
    (directory, root)
}

fn full_fold(log: &[u8]) -> State {
    let mut state = State::default();
    let mut offset = 0;
    for bytes in log.split_inclusive(|&byte| byte == b'\n') {
        let header = crate::session_manager::log::check_line(bytes).unwrap();
        let body = parse_line_body(header.kind.unwrap(), bytes).unwrap();
        state.apply(0, header.seq, offset, &body);
        offset += bytes.len() as u64;
    }
    state
}

#[test]
fn a_summary_folds_from_the_newest_snapshot_to_the_same_state_as_a_full_fold() {
    let (_directory, root) = root_with("snapshot_small");
    let peeked = read_summary(&root, "PsjMeU3Vl5Ld").unwrap();
    assert_eq!(peeked.role, Role::Root);
    assert_eq!(
        peeked.state,
        full_fold(&fixture_log("snapshot_small", "PsjMeU3Vl5Ld"))
    );
    assert_eq!(peeked.created_ms, 1_791_774_838_644);
    assert!(peeked.updated_ms > peeked.created_ms);
    assert_eq!(
        peeked.state.setting(SetKey::Workspace),
        Some(&b"\"/private/tmp/fxv2/snapshot_small/other\""[..])
    );
}

#[test]
fn every_fx_session_peeks_to_its_full_fold() {
    for (scenario, id) in [
        ("listing", "nJI52ukeF2bU"),
        ("tool", "ia8mVXaiyAf1"),
        ("kill_tool", "NYXx_508J28b"),
    ] {
        let (_directory, root) = root_with(scenario);
        let peeked = read_summary(&root, id).unwrap();
        assert_eq!(peeked.state, full_fold(&fixture_log(scenario, id)), "{id}");
    }
}

#[test]
fn a_summary_refuses_a_damaged_tail_and_a_session_without_its_first_line() {
    let (directory, root) = root_with("listing");
    let log = directory.path().join("nJI52ukeF2bU").join(LOG_NAME);
    let mut bytes = fs::read(&log).unwrap();
    let end = bytes.len();
    bytes[end - 5] ^= 1;
    fs::write(&log, &bytes).unwrap();
    assert_eq!(
        read_summary(&root, "nJI52ukeF2bU").err(),
        Some(OpenError::Corrupt)
    );
    fs::write(&log, line(1, "closed", "")).unwrap();
    assert_eq!(
        read_summary(&root, "nJI52ukeF2bU").err(),
        Some(OpenError::Corrupt)
    );
    assert_eq!(
        read_summary(&root, "missing00000").err(),
        Some(OpenError::NotFound)
    );
    assert_eq!(
        read_summary(&root, "../escape").err(),
        Some(OpenError::NotFound)
    );
    fs::set_permissions(&log, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&log).is_err() {
        assert_eq!(
            read_summary(&root, "nJI52ukeF2bU").err(),
            Some(OpenError::Fault(IoFault::AccessDenied))
        );
    }
}

#[test]
fn reading_forward_gives_every_line_and_stops_at_damage() {
    let (directory, root) = root_with("listing");
    let mut kinds = Vec::new();
    let damaged = read_forward(&root, "nJI52ukeF2bU", |seq, body| {
        kinds.push((seq, body.is_some()));
        Ok::<(), ()>(())
    })
    .unwrap()
    .unwrap();
    assert!(!damaged);
    assert_eq!(kinds.len(), 23);
    let log = directory.path().join("nJI52ukeF2bU").join(LOG_NAME);
    let mut bytes = fs::read(&log).unwrap();
    let second = bytes.iter().position(|&byte| byte == b'\n').unwrap() + 1;
    bytes[second + 3] ^= 1;
    fs::write(&log, bytes).unwrap();
    let mut seen = 0;
    let damaged = read_forward(&root, "nJI52ukeF2bU", |_, _| {
        seen += 1;
        Ok::<(), ()>(())
    })
    .unwrap()
    .unwrap();
    assert!(damaged);
    assert_eq!(seen, 1);
}
