use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use ofx_testkit::{FakeServer, PtySession, Reply, chat_text_events};
use serde_json::json;

const WAIT: Duration = Duration::from_secs(15);
const EARLIER_LINE: &str = "1700000000000 [agent] written before the trace";
const REVIEW: &str = "Review and redact it before sharing.";
const PORTKEY_KEY: &str = "pk-test-0123456789";
const BEARER: &str = "sk-bearer-secret-0123456789abcdef";
const VIRTUAL_KEY: &str = "vk-virtual-secret-9876543210";
const SECRETS: [&str; 3] = [PORTKEY_KEY, BEARER, VIRTUAL_KEY];

struct Launched {
    workspace: PathBuf,
    reports: PathBuf,
    copied: PathBuf,
    session: PtySession,
    server: FakeServer,
}

fn launch(clipboard_exit: i32) -> (tempfile::TempDir, Launched) {
    launch_with(clipboard_exit, Vec::new())
}

fn launch_with(clipboard_exit: i32, replies: Vec<Reply>) -> (tempfile::TempDir, Launched) {
    let home = tempfile::tempdir().expect("prepare the trace test");
    let root = home.path().canonicalize().expect("prepare the trace test");
    let workspace = root.join("workspace");
    let config = root.join("config/oh-fx");
    let bin = root.join("bin");
    let reports = root.join("reports");
    for path in [&workspace, &config, &bin, &reports] {
        fs::create_dir_all(path).expect("prepare the trace test");
    }
    for tool in ["pbcopy", "xclip"] {
        let script = bin.join(tool);
        fs::write(
            &script,
            format!("#!/bin/sh\ncat > \"$OH_FX_TRACE_TEST_COPY\"\nexit {clipboard_exit}\n"),
        )
        .expect("prepare the trace test");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .expect("prepare the trace test");
    }
    fs::write(workspace.join("trace.log"), format!("{EARLIER_LINE}\n"))
        .expect("prepare the trace test");
    let server = FakeServer::start(replies);
    fs::write(
        config.join("settings.json"),
        json!({
            "provider": "local",
            "providers": {"local": {
                "protocol": "openai-chat-completions",
                "base_url": server.base_url(),
                "auth": {"type": "none"},
                "models": ["model-a"]
            }}
        })
        .to_string(),
    )
    .expect("prepare the trace test");
    let copied = root.join("copied");
    let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
    command
        .current_dir(&workspace)
        .env_clear()
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm-256color")
        .env("TMPDIR", &reports)
        .env("OH_FX_AUTO_UPGRADE", "0")
        .env("OH_FX_TRACE_LOG", "trace.log")
        .env("OH_FX_TRACE_TEST_COPY", &copied)
        .process_group(0);
    let session = PtySession::spawn(command, 24, 400).expect("prepare the trace test");
    session
        .wait_for(WAIT, |screen| screen.contains("Run /help for commands"))
        .expect("prepare the trace test");
    (
        home,
        Launched {
            workspace,
            reports,
            copied,
            session,
            server,
        },
    )
}

fn saved_report(reports: &Path) -> PathBuf {
    let entries: Vec<PathBuf> = fs::read_dir(reports)
        .expect("prepare the trace test")
        .map(|entry| entry.expect("prepare the trace test").path())
        .collect();
    assert_eq!(entries.len(), 1, "{entries:?}");
    entries[0].clone()
}

fn quit(launched: &mut Launched) {
    launched.session.send(b"/quit\r");
    assert!(
        launched
            .session
            .wait_exit(WAIT)
            .expect("prepare the trace test")
            .success()
    );
    assert!(launched.server.requests().is_empty());
}

#[test]
fn trace_copies_a_private_report_with_the_trace_log_tail() {
    let (_home, mut launched) = launch(0);
    launched.session.send(b"/trace\r");
    launched
        .session
        .wait_for(WAIT, |screen| {
            screen.contains(&format!("Trace copied to clipboard. {REVIEW}"))
        })
        .unwrap();
    let saved = saved_report(&launched.reports);
    let name = saved.file_name().unwrap().to_str().unwrap().to_owned();
    assert!(name.starts_with("oh-fx-trace-"), "{name}");
    assert_eq!(
        fs::metadata(&saved).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let report = fs::read_to_string(&saved).unwrap();
    assert_eq!(fs::read_to_string(&launched.copied).unwrap(), report);
    let log = launched.workspace.join("trace.log");
    assert!(report.starts_with("# oh-fx trace\n\n"));
    assert!(report.contains(&format!(
        "\nOH_FX_TRACE: off\ntrace_log: {}\n",
        log.display()
    )));
    assert!(report.contains(&format!(
        "\n## Trace Tail\npath={} last_bytes={}\nonly obvious secrets masked\n{EARLIER_LINE}\n",
        log.display(),
        EARLIER_LINE.len() + 1
    )));
    assert!(report.contains("\nTERM: xterm-256color\n"));
    quit(&mut launched);
}

#[test]
fn a_failed_copy_names_the_saved_report() {
    let (_home, mut launched) = launch(1);
    launched.session.send(b"/trace\r");
    let expected_start = if cfg!(target_os = "macos") {
        "Clipboard copy failed. Trace saved at "
    } else {
        "Trace saved at "
    };
    let screen = launched
        .session
        .wait_for(WAIT, |screen| screen.contains(REVIEW))
        .unwrap();
    let saved = saved_report(&launched.reports);
    assert!(
        screen.contains(&format!("{expected_start}{}. {REVIEW}", saved.display())),
        "{screen}"
    );
    assert!(
        fs::read_to_string(&saved)
            .unwrap()
            .starts_with("# oh-fx trace\n\n")
    );
    quit(&mut launched);
}

#[test]
fn a_cancelled_turn_writes_its_interrupt_trace_lines() {
    let held =
        Reply::held_sse(&chat_text_events(&["First line.\nSecond line.\n", "still going"])[..3]);
    let (_home, mut launched) = launch_with(0, vec![held]);
    launched.session.send(b"slow\r");
    launched
        .session
        .wait_for(WAIT, |screen| screen.contains("First line."))
        .expect("the reply streams");
    launched.session.send(b"\x03");
    launched
        .session
        .wait_for(WAIT, |screen| screen.contains("Cancelled"))
        .expect("the turn is cancelled");
    let log = launched.workspace.join("trace.log");
    let written = launched
        .session
        .wait_for(WAIT, |_| {
            fs::read_to_string(&log).is_ok_and(|text| text.contains("event=prompt_finish"))
        })
        .map(|_| fs::read_to_string(&log).expect("read the trace log"))
        .expect("the turn finishes");
    let partial = "First line.\nSecond line.\nstill going".len();
    for expected in [
        " [worker] cancel requested processing=true queued=0 steering_pending=false\n".to_owned(),
        " [interrupt] event=cancel_requested processing=true queued=0 steering_pending=false active_tool_known=false\n".to_owned(),
        " [interrupt] event=cancel_observed turn_id=1 step_id=1 active_tool_known=false\n".to_owned(),
        format!(" [agent] interrupted marker persisted prompt_bytes=4 partial_assistant_bytes={partial} active_tool=false completed_tool_count=0 completed_tool_names=none\n"),
        format!(" [interrupt] event=interrupted_history_persisted turn_id=1 step_id=1 prompt_bytes=4 partial_assistant_bytes={partial} active_tool_known=false completed_tool_count=0 completed_tool_names=none\n"),
        format!(" [interrupt] event=interrupt_persisted turn_id=1 step_id=1 prompt_bytes=4 partial_assistant_bytes={partial} active_tool=false completed_tool_count=0 completed_tool_names=none active_tool_reason=partial_assistant_only\n"),
        " [interrupt] event=finish_event_emitted turn_id=1 step_id=1 outcome_kind=interrupted\n".to_owned(),
        " [agent] event=prompt_finish turn_id=1 outcome_kind=interrupted\n".to_owned(),
    ] {
        assert!(written.contains(&expected), "{expected}\n{written}");
    }
    launched.session.send(b"\x1b");
    launched
        .session
        .wait_for(WAIT, |screen| {
            !screen.contains("press ctrl+c again to exit")
        })
        .expect("escape disarms the exit hint");
    launched.session.send(b"/quit\r");
    assert!(
        launched
            .session
            .wait_exit(WAIT)
            .expect("the shell exits")
            .success()
    );
}

struct Traced {
    output: Output,
    log: String,
}

fn ask_traced(replies: Vec<Reply>, extra: &[&str]) -> Traced {
    let home = tempfile::tempdir().expect("prepare the trace test");
    let root = home.path().canonicalize().expect("prepare the trace test");
    let workspace = root.join("workspace");
    let config = root.join("config/oh-fx");
    for path in [&workspace, &config] {
        fs::create_dir_all(path).expect("prepare the trace test");
    }
    let server = FakeServer::start(replies);
    fs::write(
        config.join("settings.json"),
        json!({
            "provider": "portkey",
            "model": "@openai/gpt-4o",
            "providers": {"portkey": {
                "protocol": "openai-chat-completions",
                "base_url": server.base_url(),
                "auth": {"type": "bearer", "env": "PORTKEY_BEARER"},
                "headers": {
                    "x-portkey-api-key": "${PORTKEY_API_KEY}",
                    "x-portkey-virtual-key": "${PORTKEY_VIRTUAL_KEY}"
                },
                "models": ["@openai/gpt-4o"]
            }}
        })
        .to_string(),
    )
    .expect("prepare the trace test");
    let log = root.join("trace.log");
    let output = Command::new(env!("CARGO_BIN_EXE_oh-fx"))
        .args(["ask"])
        .args(extra)
        .current_dir(&workspace)
        .env_clear()
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("SHELL", "/bin/sh")
        .env("OH_FX_AUTO_UPGRADE", "0")
        .env("OH_FX_TRACE_LOG", &log)
        .env("PORTKEY_API_KEY", PORTKEY_KEY)
        .env("PORTKEY_BEARER", BEARER)
        .env("PORTKEY_VIRTUAL_KEY", VIRTUAL_KEY)
        .stdin(Stdio::null())
        .output()
        .expect("run oh-fx ask");
    let log = fs::read_to_string(&log).expect("read the trace log");
    let seen = server.requests();
    assert!(
        seen.iter().all(|request| request.header("authorization")
            == Some(&format!("Bearer {BEARER}"))
            && request.header("x-portkey-virtual-key") == Some(VIRTUAL_KEY)),
        "the requests carry every credential"
    );
    Traced { output, log }
}

fn secret_body() -> String {
    json!({"error": {"message": format!(
        "denied for {PORTKEY_KEY} with Bearer {BEARER} and {VIRTUAL_KEY}"
    )}})
    .to_string()
}

fn cut_off_after(text: &str) -> Reply {
    let body: String = chat_text_events(&[text])[..2]
        .iter()
        .flat_map(|event| ["data: ", event, "\n\n"])
        .collect();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len() + 1
    );
    Reply::Raw(format!("{head}{body}").into_bytes())
}

fn bodies(log: &str) -> Vec<&str> {
    log.lines()
        .map(|line| line.split_once(' ').expect("a timestamped line").1)
        .collect()
}

fn assert_no_secret(log: &str) {
    for secret in SECRETS {
        assert!(
            !log.contains(secret),
            "{secret} reached the trace log:\n{log}"
        );
    }
}

#[test]
fn a_recovered_ask_writes_its_stream_error_and_recovery_lines_without_credentials() {
    let traced = ask_traced(
        vec![
            cut_off_after("Hel"),
            Reply::status(503, secret_body()),
            Reply::sse(&chat_text_events(&["Hello."])),
        ],
        &["--json", "hi"],
    );
    let result: serde_json::Value = serde_json::from_slice(&traced.output.stdout).unwrap();
    assert_eq!(result["final_output"], "Hello.", "{result}");
    let lines = bodies(&traced.log);
    let wanted = [
        "[gateway] event=stream_error turn_id=1 step_id=1 err=ReadFailed cancel_requested=false provider_attempts=1/10 saw_content=true saw_tool_start=false saw_provider_tool_start=false recovery=continue_response replay_safe=false retry=true",
        "[agent] event=recovery_checkpoint_set turn_id=1 step_id=1 provider_attempts=1/10 outstanding=false cause=transport_interrupted action=continue_response",
        "[agent] restarting response preview_bytes=3 superseded_preview_bytes=0",
        "[agent] event=recovery_checkpoint_set turn_id=1 step_id=1 provider_attempts=2/10 outstanding=false cause=provider_unavailable action=continue_response",
        "[agent] event=prompt_finish turn_id=1 outcome_kind=assistant",
    ];
    let mut at = 0;
    for line in wanted {
        let found = lines[at..]
            .iter()
            .position(|seen| *seen == line)
            .unwrap_or_else(|| panic!("{line}\n{}", traced.log));
        at += found + 1;
    }
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("event=stream_error"))
            .count(),
        1,
        "{}",
        traced.log
    );
    assert_no_secret(&traced.log);
}

#[test]
fn a_provider_error_in_the_stream_writes_its_route_failure_without_credentials() {
    let traced = ask_traced(
        vec![Reply::sse(&[secret_body()])],
        &["--json", "--no-save", "hi"],
    );
    assert_eq!(traced.output.status.code(), Some(1));
    let lines = bodies(&traced.log);
    let route = lines
        .iter()
        .find(|line| line.starts_with("[agent] event=route_failure "))
        .unwrap_or_else(|| panic!("{}", traced.log));
    assert!(
        route.starts_with("[agent] event=route_failure turn_id=1 step_id=1 selected_model=@openai/gpt-4o route=@openai/gpt-4o fast_mode=false semantic_attempt=1/10 http_status=200 finish_reason=error saw_content=false saw_tool_start=false retry=false detail="),
        "{route}"
    );
    assert!(route.contains("denied for"), "{route}");
    assert!(lines.contains(
        &"[agent] event=provider_completion_failed turn_id=1 step_id=1 finish_reason=error content_bytes=0 tool_call_count=0"
    ), "{}", traced.log);
    assert!(
        lines.contains(&"[agent] event=prompt_finish turn_id=1 outcome_kind=provider_error"),
        "{}",
        traced.log
    );
    assert!(!traced.log.contains("event=stream_error"), "{}", traced.log);
    assert_no_secret(&traced.log);
}

#[test]
fn trace_reports_the_turns_network_calls() {
    let (_home, mut launched) = launch_with(
        0,
        vec![
            Reply::status(503, r#"{"error":{"message":"busy"}}"#),
            Reply::sse(&chat_text_events(&["All done."])),
        ],
    );
    launched.session.send(b"hello\r");
    launched
        .session
        .wait_for(WAIT, |screen| screen.contains("All done."))
        .expect("the retried turn answers");
    launched.session.send(b"/trace\r");
    launched
        .session
        .wait_for(WAIT, |screen| {
            screen.contains(&format!("Trace copied to clipboard. {REVIEW}"))
        })
        .expect("the report is copied");
    let report = fs::read_to_string(saved_report(&launched.reports)).unwrap();
    let network = report
        .split("\n## Network Calls\n")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .unwrap_or_else(|| panic!("{report}"));
    assert!(
        network.starts_with("last=2 ok=1 errors=1 avg="),
        "{network}"
    );
    assert!(
        network.contains("\nsession: calls=2 ok=1 errors=1 total_time="),
        "{network}"
    );
    assert!(
        network.contains("\ncoverage: complete (window holds every recorded call)\nturns:\n  turn 1: calls=2 errors=1 "),
        "{network}"
    );
    let calls: Vec<&str> = network
        .lines()
        .filter(|line| line.starts_with('['))
        .map(|line| line.split_once("] ").expect("a timestamped call").1)
        .collect();
    assert_eq!(calls.len(), 2, "{network}");
    assert!(
        calls[0].starts_with("model=model-a source=parent status=503 duration="),
        "{network}"
    );
    assert!(calls[0].ends_with(" turn=1 step=1"), "{network}");
    assert!(
        calls[1].starts_with("model=model-a source=parent status=200 duration="),
        "{network}"
    );
    assert!(
        calls[1].ends_with(" stop_reason=stop turn=1 step=1"),
        "{network}"
    );
    let problems = report
        .split("\n## Problems\n")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .unwrap();
    assert!(
        problems.contains("] model=model-a source=parent status=503 "),
        "{problems}"
    );
    assert!(problems.starts_with("- network ["), "{problems}");
    launched.session.send(b"/quit\r");
    assert!(
        launched
            .session
            .wait_exit(WAIT)
            .expect("the shell exits")
            .success()
    );
}
