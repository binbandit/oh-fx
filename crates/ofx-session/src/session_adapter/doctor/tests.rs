use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process;

use ofx_config::PrivateDir;

use super::*;
use crate::session_manager::test_support::install;

const A: &str = "nJI52ukeF2bU";
const D: &str = "hDoX4WdPFWDK";

fn manager(path: &Path) -> Manager {
    Manager::new(PrivateDir::open_existing(path).unwrap().unwrap())
}

fn break_log(root: &Path, id: &str) {
    let log = root.join(id).join("log.jsonl");
    let mut bytes = fs::read(&log).unwrap();
    let at = bytes.iter().position(|&byte| byte == b'\n').unwrap() + 20;
    bytes[at] ^= 0x20;
    fs::write(&log, bytes).unwrap();
}

#[test]
fn doctor_verifies_each_session_and_names_the_newest() {
    let directory = tempfile::tempdir().unwrap();
    install("listing", directory.path());
    let report = doctor(&manager(directory.path()), 64).unwrap();
    assert_eq!(
        report,
        DoctorReport {
            sessions: 4,
            checked: 4,
            latest: Some(D.to_owned()),
            damaged: Vec::new(),
        }
    );
    break_log(directory.path(), A);
    let report = doctor(&manager(directory.path()), 64).unwrap();
    assert_eq!(report.damaged, [A]);
}

#[test]
fn doctor_stops_verifying_at_its_limit_but_counts_every_session() {
    let directory = tempfile::tempdir().unwrap();
    install("listing", directory.path());
    break_log(directory.path(), A);
    let report = doctor(&manager(directory.path()), 2).unwrap();
    assert_eq!((report.sessions, report.checked), (4, 2));
    assert!(report.damaged.is_empty());
}

const TRACE_CHILD: &str = "OH_FX_SESSIONS_V2_DOCTOR_TRACE_CHILD";
const TRACE_TEST: &str =
    "session_adapter::doctor::tests::an_unreadable_session_is_damaged_and_traced";

#[test]
fn an_unreadable_session_is_damaged_and_traced() {
    if let Some(root) = std::env::var_os(TRACE_CHILD) {
        ofx_trace::configure_from_env(Path::new(&root), None);
        let sessions = Path::new(&root).join("sessions");
        install("listing", &sessions);
        let log = sessions.join(A).join("log.jsonl");
        fs::set_permissions(&log, fs::Permissions::from_mode(0o000)).unwrap();
        let report = doctor(&manager(&sessions), 64).unwrap();
        assert_eq!(report.damaged, [A]);
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let probe = root.path().join("probe");
    fs::write(&probe, "").unwrap();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&probe).is_ok() {
        return;
    }
    let log = root.path().join("trace.log");
    let status = process::Command::new(std::env::current_exe().unwrap())
        .args([TRACE_TEST, "--exact", "--test-threads=1"])
        .env_clear()
        .env(TRACE_CHILD, root.path())
        .env("OH_FX_TRACE_LOG", &log)
        .stdout(process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "{status:?}");
    let written = fs::read_to_string(&log).unwrap();
    let line =
        format!(" [session] event=sessions_v2_doctor_unreadable session={A} err=AccessDenied\n");
    assert!(written.contains(&line), "{written}");
}
