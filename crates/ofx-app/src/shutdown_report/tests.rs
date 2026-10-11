use std::fs;
use std::os::unix::fs::PermissionsExt;

use serde_json::Value;

use super::*;

const RECORDED_AT_MS: i64 = 1_700_000_000_000;

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn names(report: &Value) -> Vec<&str> {
    report["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|stage| stage["name"].as_str().unwrap())
        .collect()
}

#[test]
fn stages_advance_monotonically_in_the_order_they_are_marked() {
    let trace = ShutdownTrace::default();
    trace.mark("first");
    trace.mark("second");
    let report: Value = serde_json::from_str(&trace.report(RECORDED_AT_MS)).unwrap();
    assert_eq!(report["version"], 1);
    assert_eq!(report["recorded_at_ms"], RECORDED_AT_MS);
    assert_eq!(names(&report), ["first", "second"]);
    let stages = report["stages"].as_array().unwrap();
    assert_eq!(stages[0]["total_ms"], stages[0]["step_ms"]);
    let total = report["total_ms"].as_u64().unwrap();
    assert!(stages[1]["total_ms"].as_u64().unwrap() <= total);
}

#[test]
fn the_report_keeps_upstreams_field_order_and_bounds_its_stages() {
    let trace = ShutdownTrace::default();
    for _ in 0..MAX_STAGES + 4 {
        trace.mark("stage");
    }
    let report = trace.report(RECORDED_AT_MS);
    assert!(
        report.starts_with("{\"version\":1,\"recorded_at_ms\":1700000000000,\"total_ms\":"),
        "{report}"
    );
    assert!(report.contains(",\"stages\":[{\"name\":\"stage\",\"step_ms\":"));
    let parsed: Value = serde_json::from_str(&report).unwrap();
    assert_eq!(names(&parsed).len(), MAX_STAGES);
    assert_eq!(
        ShutdownTrace::default().report(RECORDED_AT_MS),
        "{\"version\":1,\"recorded_at_ms\":1700000000000,\"total_ms\":0,\"stages\":[]}"
    );
}

#[test]
fn the_report_is_saved_privately_under_the_state_directory_and_read_back() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("state/oh-fx");
    assert_eq!(read_last(&state), None);
    let trace = ShutdownTrace::default();
    trace.mark("complete");
    trace.save(&state, RECORDED_AT_MS);
    let diagnostics = state.join(DIRECTORY);
    let saved = diagnostics.join(FILE);
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(&diagnostics), 0o700);
    assert_eq!(mode(&saved), 0o600);
    let contents = read_last(&state).unwrap();
    assert_eq!(contents, trace.report(RECORDED_AT_MS).into_bytes());
    let later = ShutdownTrace::default();
    later.save(&state, RECORDED_AT_MS + 1);
    assert_eq!(
        read_last(&state).unwrap(),
        later.report(RECORDED_AT_MS + 1).into_bytes()
    );
    assert_eq!(fs::read_dir(&diagnostics).unwrap().count(), 1);
}

#[test]
fn an_oversized_or_unwritable_report_is_left_alone() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("state");
    fs::create_dir_all(state.join(DIRECTORY)).unwrap();
    let oversized = vec![b' '; usize::try_from(MAX_REPORT_BYTES).unwrap() + 1];
    fs::write(state.join(DIRECTORY).join(FILE), oversized).unwrap();
    assert_eq!(read_last(&state), None);

    let blocked = home.path().join("blocked");
    fs::write(&blocked, "a file where the state directory belongs").unwrap();
    ShutdownTrace::default().save(&blocked, RECORDED_AT_MS);
    assert!(blocked.is_file());
}

#[test]
fn only_a_joined_shutdown_is_marked_complete() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("state");
    let timed_out = ShutdownTrace::default();
    timed_out.begin();
    timed_out.mark("worker_thread_join_timed_out");
    timed_out.finish(false, Some(&state), RECORDED_AT_MS);
    let saved: Value = serde_json::from_slice(&read_last(&state).unwrap()).unwrap();
    assert_eq!(names(&saved), ["worker_thread_join_timed_out"]);
    let joined = ShutdownTrace::default();
    joined.finish(true, Some(&state), RECORDED_AT_MS);
    let saved: Value = serde_json::from_slice(&read_last(&state).unwrap()).unwrap();
    assert_eq!(names(&saved), ["complete"]);
    ShutdownTrace::default().finish(true, None, RECORDED_AT_MS);
}
