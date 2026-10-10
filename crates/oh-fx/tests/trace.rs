use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ofx_testkit::{FakeServer, PtySession, Reply, chat_text_events};
use serde_json::json;

const WAIT: Duration = Duration::from_secs(15);
const EARLIER_LINE: &str = "1700000000000 [agent] written before the trace";
const REVIEW: &str = "Review and redact it before sharing.";

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
