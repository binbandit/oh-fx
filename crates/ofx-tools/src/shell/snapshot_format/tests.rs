use ofx_exec::{CommandStatus, Snapshot, SnapshotState};
use serde_json::Value;

use super::*;

const ASCII_GOLDEN: &str = r#"{"session_id":"shell-7","state":"completed","backend":"captured","persistence":"process","output_truncated":true,"output_incomplete":false,"output_terminal_safe":true,"full_output_handle":null,"exit_code":0,"signal":null,"termination_indeterminate":false,"duration_ms":42,"accepted_bytes":null,"error":null,"retry_guidance":null,"output_delta":"HEAD_SENTINEL\nabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxy\n... bytes omitted ...\nlmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghij\nTAIL_SENTINEL"}"#;
const HOSTILE_GOLDEN: &str = r#"{"session_id":null,"state":"completed","backend":"captured","persistence":"process","output_truncated":true,"output_incomplete":false,"output_terminal_safe":true,"full_output_handle":null,"exit_code":1,"signal":null,"termination_indeterminate":false,"duration_ms":null,"accepted_bytes":null,"error":null,"retry_guidance":null,"output_delta":"\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\x0a... bytes omitted ...\\x0a\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\x0aCONTROL_TAIL"}"#;
const UTF8_GOLDEN: &str = r#"{"session_id":"shell-9","state":"running","backend":"captured","persistence":"process","output_truncated":true,"output_incomplete":false,"output_terminal_safe":true,"full_output_handle":null,"exit_code":null,"signal":null,"termination_indeterminate":false,"duration_ms":null,"accepted_bytes":null,"error":null,"retry_guidance":null,"output_delta":"é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀\n... bytes omitted ...\né€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀é€😀"}"#;
const MIXED_GOLDEN: &str = r#"{"session_id":"shell-10","state":"stopped","backend":"captured","persistence":"process","output_truncated":true,"output_incomplete":false,"output_terminal_safe":true,"full_output_handle":null,"exit_code":null,"signal":15,"termination_indeterminate":false,"duration_ms":null,"accepted_bytes":null,"error":"TimeoutExpired","retry_guidance":null,"output_delta":"\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\n... bytes omitted ...\ne <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n\u001b[32mok\u001b[0m line <ZWSP>\r\n"}"#;
const TINY_GOLDEN: &str = r#"{"session_id":"shell-11","state":"lost","backend":"captured","persistence":"process","output_truncated":true,"output_incomplete":true,"output_terminal_safe":true,"full_output_handle":null,"exit_code":null,"signal":null,"termination_indeterminate":true,"duration_ms":null,"accepted_bytes":null,"error":null,"retry_guidance":"Command output is incomplete. Inspect external state and available output before retrying; do not blindly rerun a command that may have changed state.","output_delta":""}"#;

fn snapshot(state: SnapshotState, output: &[u8]) -> Snapshot {
    Snapshot {
        execution_id: "shell-1".to_owned(),
        command: "command".to_owned(),
        cwd: "/tmp".into(),
        retained: false,
        state,
        output_delta: output.to_vec(),
        output_truncated: false,
        output_incomplete: false,
        duration_ms: None,
        stdout_bytes: output.len(),
        stderr_bytes: 0,
        error_name: None,
    }
}

fn formatted(snapshot: &Snapshot, limit: usize) -> Value {
    serde_json::from_str(&format_snapshot(snapshot, limit)).unwrap()
}

fn completed(code: i64) -> SnapshotState {
    SnapshotState::Completed(CommandStatus::ExitCode(code))
}

#[test]
fn bounded_projections_match_the_upstream_formatter_byte_for_byte() {
    let mut ascii = b"HEAD_SENTINEL\n".to_vec();
    ascii.extend((0..3000_usize).map(|index| b'a' + u8::try_from(index % 26).unwrap()));
    ascii.extend_from_slice(b"\nTAIL_SENTINEL");
    let ascii = Snapshot {
        execution_id: "shell-7".to_owned(),
        retained: true,
        duration_ms: Some(42),
        ..snapshot(completed(0), &ascii)
    };
    assert_eq!(format_snapshot(&ascii, 1024), ASCII_GOLDEN);

    let mut hostile = vec![0xff; 2000];
    hostile.extend_from_slice(b"\nCONTROL_TAIL");
    let hostile_snapshot = Snapshot {
        execution_id: "shell-8".to_owned(),
        ..snapshot(completed(1), &hostile)
    };
    assert_eq!(format_snapshot(&hostile_snapshot, 1024), HOSTILE_GOLDEN);

    let utf8 = Snapshot {
        execution_id: "shell-9".to_owned(),
        retained: true,
        output_truncated: true,
        ..snapshot(SnapshotState::Running, "é€😀".repeat(700).as_bytes())
    };
    assert_eq!(format_snapshot(&utf8, 1024), UTF8_GOLDEN);

    let mut mixed = Vec::new();
    for index in 0..300 {
        mixed.extend_from_slice(b"\x1b[32mok\x1b[0m line ");
        if index == 150 {
            mixed.extend_from_slice(b"\0\xc3");
        }
        mixed.extend_from_slice("\u{200b}\r\n".as_bytes());
    }
    let mixed = Snapshot {
        execution_id: "shell-10".to_owned(),
        retained: true,
        error_name: Some("TimeoutExpired"),
        ..snapshot(
            SnapshotState::Stopped(Some(CommandStatus::Signal(15))),
            &mixed,
        )
    };
    assert_eq!(
        format_snapshot(&mixed, 1024),
        MIXED_GOLDEN.replace("<ZWSP>", "\u{200b}")
    );

    let tiny = Snapshot {
        execution_id: "shell-11".to_owned(),
        retained: true,
        output_incomplete: true,
        ..snapshot(SnapshotState::Lost, &hostile)
    };
    assert_eq!(format_snapshot(&tiny, 300), TINY_GOLDEN);
}

#[test]
fn lost_shell_snapshot_preserves_indeterminate_execution_guidance() {
    let object = formatted(
        &Snapshot {
            retained: true,
            ..snapshot(SnapshotState::Lost, b"")
        },
        64 * 1024,
    );
    assert_eq!(object["termination_indeterminate"], true);
    assert!(
        object["retry_guidance"]
            .as_str()
            .unwrap()
            .contains("do not blindly rerun")
    );
}

#[test]
fn completed_shell_snapshot_reports_incomplete_output_without_losing_status() {
    let object = formatted(
        &Snapshot {
            output_incomplete: true,
            ..snapshot(completed(0), b"partial output")
        },
        64 * 1024,
    );
    assert_eq!(object["state"], "completed");
    assert_eq!(object["exit_code"], 0);
    assert_eq!(object["output_incomplete"], true);
    assert!(
        object["retry_guidance"]
            .as_str()
            .unwrap()
            .contains("do not blindly rerun")
    );
}

#[test]
fn completed_shell_snapshots_carry_parse_and_usage_guidance() {
    let parse = formatted(&snapshot(completed(1), b"zsh:1: unmatched '\n"), 64 * 1024);
    assert_eq!(parse["exit_code"], 1);
    assert_eq!(parse["retry_guidance"], SHELL_PARSE_RETRY_GUIDANCE);
    let usage = formatted(
        &snapshot(
            completed(2),
            b"usage: grep [-abcdDEFGHhIiJLlMmnOopqRSsUVvwXxZz] [-A num] [-B num]\n\t[-e pattern] [-f file] [--binary-files=value]\n",
        ),
        64 * 1024,
    );
    assert_eq!(usage["retry_guidance"], USAGE_ERROR_RETRY_GUIDANCE);
    let ordinary = formatted(
        &snapshot(
            completed(2),
            b"grep: /nonexistent: No such file or directory\n",
        ),
        64 * 1024,
    );
    assert_eq!(ordinary["retry_guidance"], Value::Null);
    let encoded = formatted(
        &snapshot(
            completed(2),
            b"usage: grep [-abcdDEFGHhIiJLlMmnOopqRSsUVvwXxZz]\npartial \0 binary\n",
        ),
        64 * 1024,
    );
    assert_eq!(encoded["retry_guidance"], USAGE_ERROR_RETRY_GUIDANCE);
    let incomplete = formatted(
        &Snapshot {
            output_incomplete: true,
            ..snapshot(
                completed(2),
                b"usage: grep [-abcdDEFGHhIiJLlMmnOopqRSsUVvwXxZz]\n",
            )
        },
        64 * 1024,
    );
    assert!(
        incomplete["retry_guidance"]
            .as_str()
            .unwrap()
            .contains("do not blindly rerun")
    );
}

#[test]
fn failure_guidance_detectors_match_only_their_signatures() {
    for (code, output, expected) in [
        (1, "zsh:1: unmatched '\n", Some(SHELL_PARSE_RETRY_GUIDANCE)),
        (
            2,
            "/bin/bash: -c: line 1: syntax error: unexpected end of file\n",
            Some(SHELL_PARSE_RETRY_GUIDANCE),
        ),
        (
            2,
            "bash: -c: line 1: syntax error near unexpected token `)'\n",
            Some(SHELL_PARSE_RETRY_GUIDANCE),
        ),
        (
            2,
            "/bin/dash: 1: Syntax error: Unterminated quoted string\n",
            Some(SHELL_PARSE_RETRY_GUIDANCE),
        ),
        (
            1,
            "zsh: parse error near '\\n'\n",
            Some(SHELL_PARSE_RETRY_GUIDANCE),
        ),
        (
            2,
            "usage: grep [-abcdDEFGHhIiJLlMmnOopqRSsUVvwXxZz]\n",
            Some(USAGE_ERROR_RETRY_GUIDANCE),
        ),
        (
            64,
            "Usage: ls [-ABCFGHabcdfghiklmnopqrstuvwx1] [file ...]\n",
            Some(USAGE_ERROR_RETRY_GUIDANCE),
        ),
        (
            2,
            "grep: unknown option\nusage: grep [-abcdDEFGHhIiJLlMmnOopqRSsUVvwXxZz]\n",
            Some(USAGE_ERROR_RETRY_GUIDANCE),
        ),
        (0, "usage: grep\n", None),
        (
            1,
            "main.c:4:5: error: expected ';' after expression (syntax error)\n",
            None,
        ),
        (1, "error: bash: syntax error\n", None),
        (2, "grep: /nonexistent: No such file or directory\n", None),
        (1, "", None),
        (
            1,
            "line one\nline two\nline three\nline four\nusage: not a banner\n",
            None,
        ),
    ] {
        assert_eq!(
            failure_retry_guidance(Some(code), output.as_bytes()),
            expected,
            "{output:?}"
        );
    }
    assert_eq!(failure_retry_guidance(None, b"zsh:1: unmatched '\n"), None);
}

#[test]
fn shell_snapshot_keeps_bounded_head_tail_and_control_metadata() {
    let mut output = b"HEAD_SENTINEL\n".to_vec();
    output.extend(std::iter::repeat_n(b'x', 70 * 1024));
    output.extend_from_slice(b"\nTAIL_SENTINEL");
    let body = format_snapshot(
        &Snapshot {
            retained: true,
            ..snapshot(completed(0), &output)
        },
        64 * 1024,
    );
    assert!(body.len() <= 16 * 1024);
    let object: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(object["full_output_handle"], Value::Null);
    assert_eq!(object["output_truncated"], true);
    let projected = object["output_delta"].as_str().unwrap();
    assert!(projected.contains("HEAD_SENTINEL"));
    assert!(projected.contains("TAIL_SENTINEL"));
    assert!(projected.contains("\n... bytes omitted ...\n"));
}

#[test]
fn shell_snapshot_projects_hostile_bytes_as_readable_terminal_safe_text() {
    let object = formatted(
        &snapshot(
            completed(0),
            b"\x1b[31mRED\x1b[0m\rREWRITE\t\0\xff\nCONTROL_TAIL\n",
        ),
        64 * 1024,
    );
    assert_eq!(object["output_terminal_safe"], true);
    let output = object["output_delta"].as_str().unwrap();
    for expected in ["\\x1b", "\\x00", "\\xff", "CONTROL_TAIL"] {
        assert!(output.contains(expected), "{expected}");
    }
}

#[test]
fn shell_snapshot_keeps_a_hostile_output_tail_within_the_result_limit() {
    let mut output = vec![0xff; 70 * 1024];
    output.extend_from_slice(b"\nCONTROL_TAIL");
    let body = format_snapshot(&snapshot(completed(0), &output), 64 * 1024);
    assert!(body.len() <= 16 * 1024);
    let object: Value = serde_json::from_str(&body).unwrap();
    let output = object["output_delta"].as_str().unwrap();
    assert!(output.contains("\\xff"));
    assert!(output.contains("bytes omitted"));
    assert!(output.contains("CONTROL_TAIL"));
    assert_eq!(object["output_truncated"], true);
}

#[test]
fn running_shell_snapshot_leaves_continuation_intent_to_the_caller() {
    let object = formatted(
        &Snapshot {
            execution_id: "shell-running".to_owned(),
            retained: true,
            ..snapshot(SnapshotState::Running, b"")
        },
        64 * 1024,
    );
    assert_eq!(object.get("next_action"), None);
    assert_eq!(object["session_id"], "shell-running");
    assert_eq!(object["state"], "running");
}

#[test]
fn result_keys_keep_the_upstream_order() {
    assert_eq!(
        format_snapshot(&snapshot(completed(0), b"ok\n"), 64 * 1024),
        r#"{"session_id":null,"state":"completed","backend":"captured","persistence":"process","output_truncated":false,"output_incomplete":false,"output_terminal_safe":true,"full_output_handle":null,"exit_code":0,"signal":null,"termination_indeterminate":false,"duration_ms":null,"accepted_bytes":null,"error":null,"retry_guidance":null,"output_delta":"ok\n"}"#
    );
}

#[test]
fn shell_stop_fails_closed_only_for_lost_or_indeterminate_outcomes() {
    assert!(stop_result_failed(SnapshotState::Lost));
    assert!(stop_result_failed(SnapshotState::Stopped(Some(
        CommandStatus::Indeterminate
    ))));
    assert!(!stop_result_failed(SnapshotState::Stopped(Some(
        CommandStatus::Signal(9)
    ))));
    assert!(!stop_result_failed(SnapshotState::Stopped(None)));
    assert!(!stop_result_failed(completed(0)));
}

#[test]
fn command_failures_follow_the_upstream_status_rules() {
    assert!(!snapshot_failed(&snapshot(SnapshotState::Running, b"")));
    assert!(!snapshot_failed(&snapshot(completed(0), b"")));
    assert!(snapshot_failed(&snapshot(completed(3), b"")));
    assert!(snapshot_failed(&snapshot(
        SnapshotState::Completed(CommandStatus::Signal(9)),
        b""
    )));
    assert!(snapshot_failed(&snapshot(
        SnapshotState::Stopped(None),
        b""
    )));
    assert!(snapshot_failed(&snapshot(SnapshotState::Lost, b"")));
    assert!(snapshot_failed(&Snapshot {
        output_incomplete: true,
        ..snapshot(completed(0), b"")
    }));
}

#[test]
fn runtime_failures_name_the_error() {
    assert_eq!(
        runtime_failure("ExecutionNotFound"),
        r#"{"error":{"tool":"shell","code":"ExecutionNotFound","retryable":false}}"#
    );
}

#[test]
fn finished_snapshots_report_upstream_command_results_byte_for_byte() {
    let built = Snapshot {
        command: "printf 'built\\n'; exit 3".to_owned(),
        cwd: "/work/space".into(),
        duration_ms: Some(12),
        stdout_bytes: 6,
        ..snapshot(completed(3), b"built\n")
    };
    assert_eq!(
        command_result(&built).as_deref(),
        Some(
            r#"{"kind":"command","command":"printf 'built\\n'; exit 3","cwd":"/work/space","exit_code":3,"signal":null,"timed_out":false,"duration_ms":12,"stdout_bytes":6,"stderr_bytes":0,"truncated":false,"output_file":null,"stdout_file":null,"stderr_file":null}"#
        )
    );
    let timed_out = Snapshot {
        command: "sleep 60".to_owned(),
        output_truncated: true,
        stdout_bytes: 0,
        error_name: Some("TimeoutExpired"),
        ..snapshot(SnapshotState::Stopped(None), b"")
    };
    assert_eq!(
        command_result(&timed_out).as_deref(),
        Some(
            r#"{"kind":"command","command":"sleep 60","cwd":"/tmp","exit_code":null,"signal":null,"timed_out":true,"duration_ms":null,"stdout_bytes":0,"stderr_bytes":0,"truncated":true,"output_file":null,"stdout_file":null,"stderr_file":null}"#
        )
    );
    let lost = Snapshot {
        command: "x\u{1b}\"y".to_owned(),
        cwd: "/tmp/é".into(),
        output_incomplete: true,
        stdout_bytes: 70_000,
        stderr_bytes: 3,
        ..snapshot(SnapshotState::Lost, b"")
    };
    assert_eq!(
        command_result(&lost).as_deref(),
        Some(
            r#"{"kind":"command","command":"x\u001b\"y","cwd":"/tmp/é","exit_code":null,"signal":null,"timed_out":false,"termination_indeterminate":true,"output_incomplete":true,"duration_ms":null,"stdout_bytes":70000,"stderr_bytes":3,"truncated":false,"output_file":null,"stdout_file":null,"stderr_file":null}"#
        )
    );
    let killed = Snapshot {
        command: "kill -9 $$".to_owned(),
        cwd: "/".into(),
        duration_ms: Some(0),
        stdout_bytes: 0,
        ..snapshot(SnapshotState::Stopped(Some(CommandStatus::Signal(9))), b"")
    };
    assert_eq!(
        command_result(&killed).as_deref(),
        Some(
            r#"{"kind":"command","command":"kill -9 $$","cwd":"/","exit_code":null,"signal":9,"timed_out":false,"duration_ms":0,"stdout_bytes":0,"stderr_bytes":0,"truncated":false,"output_file":null,"stdout_file":null,"stderr_file":null}"#
        )
    );
    assert_eq!(
        command_result(&snapshot(SnapshotState::Running, b"partial")),
        None
    );
}

#[test]
fn process_presentations_name_only_settled_failures_and_timeouts() {
    let presented = |state| process_presentation(&snapshot(state, b""));
    assert_eq!(presented(completed(0)), None);
    assert_eq!(
        presented(completed(7)),
        Some(CommandProcessPresentation::ExitCode(7))
    );
    assert_eq!(
        presented(SnapshotState::Completed(CommandStatus::Signal(9))),
        Some(CommandProcessPresentation::Signal(9))
    );
    assert_eq!(
        presented(SnapshotState::Completed(CommandStatus::Indeterminate)),
        None
    );
    assert_eq!(
        presented(SnapshotState::Stopped(Some(CommandStatus::Signal(15)))),
        None
    );
    assert_eq!(presented(SnapshotState::Running), None);
    assert_eq!(presented(SnapshotState::Lost), None);
    let timed_out = Snapshot {
        error_name: Some(TIMEOUT_EXPIRED),
        ..snapshot(SnapshotState::Stopped(Some(CommandStatus::Signal(15))), b"")
    };
    assert_eq!(
        process_presentation(&timed_out),
        Some(CommandProcessPresentation::TimedOut)
    );
}
