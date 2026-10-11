use std::env;
use std::process::Command;

use super::*;
use crate::session_discovery::{Classification, classify_session};
use crate::session_display_metadata::read_sidecar_title;
use crate::session_event::encode_conversation_frame;

const CHILD: &str = "OFX_SESSION_TRACE_LINES_CHILD";
const TEST: &str =
    "session_migration::tests::trace_lines::session_lines_reach_a_configured_trace_log";

#[test]
fn session_lines_reach_a_configured_trace_log() {
    if let Some(root) = env::var_os(CHILD) {
        write_session_lines(Path::new(&root));
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("trace.log");
    let status = Command::new(env::current_exe().unwrap())
        .args([TEST, "--exact", "--test-threads=1"])
        .env_clear()
        .env(CHILD, root.path())
        .env("OH_FX_TRACE_LOG", &log)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "{status:?}");
    let written = fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = written
        .lines()
        .filter_map(|line| line.split_once(' ').map(|(_, body)| body))
        .collect();
    for wanted in [
        "[session] event=persisted_tool_arguments_repaired source=schema_v3 call_id=call_1 tool=",
        "[session] schema_v3 log replay failed id=legacy-traced-frames err=",
        "[session] event=display_sidecar_invalid err=InvalidDisplayMetadata",
        "[session] event=display_sidecar_unreadable err=",
        "[session] conversation event rejected rule=envelope-header",
    ] {
        assert!(
            lines.iter().any(|line| line.starts_with(wanted)),
            "{wanted}\n{written}"
        );
    }
    let repaired = lines
        .iter()
        .find(|line| line.contains("persisted_tool_arguments_repaired"))
        .unwrap();
    assert!(
        repaired.ends_with(" failure=malformed_json paired_result=true"),
        "{written}"
    );
    assert!(!written.contains("{bad"), "{written}");
}

fn write_session_lines(root: &Path) {
    ofx_trace::configure_from_env(root, None);
    let fixture = Fixture::new();
    let repaired = command_turn(
        "list",
        "call_1",
        &command_result("call_1", "listing", "null"),
        "done",
    )
    .replace("{\\\"command\\\":\\\"ls\\\"}", "{bad");
    fixture.converted(&LegacyLog::started("legacy-traced-repair", "/work").turn(&repaired));
    let broken = LegacyLog::started("legacy-traced-frames", "/work").frame("unknown_kind", "{}");
    broken.write(fixture.root.path());
    let sessions = PrivateDir::open_existing(fixture.root.path())
        .unwrap()
        .unwrap();
    assert!(classify_session(&sessions, &broken.id, Classification::Resume).is_err());
    let untitled = LegacyLog::started("legacy-traced-untitled", "/work").titled("");
    assert_eq!(read_sidecar_title(&fixture.dir(&untitled)), None);
    let oversized = LegacyLog::started("legacy-traced-oversized", "/work").titled("listed");
    let dir = fixture.dir(&oversized);
    fs::write(
        fixture.root.path().join(&oversized.id).join("display.json"),
        "x".repeat(64 * 1024),
    )
    .unwrap();
    assert_eq!(read_sidecar_title(&dir), None);
    assert!(encode_conversation_frame(0, 1, &user("hello")).is_err());
}
