use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ofx_testkit::{FakeServer, PtySession};
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
    let home = tempfile::tempdir().unwrap();
    let root = home.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    let config = root.join("config/oh-fx");
    let bin = root.join("bin");
    let reports = root.join("reports");
    for path in [&workspace, &config, &bin, &reports] {
        fs::create_dir_all(path).unwrap();
    }
    for tool in ["pbcopy", "xclip"] {
        let script = bin.join(tool);
        fs::write(
            &script,
            format!("#!/bin/sh\ncat > \"$OH_FX_TRACE_TEST_COPY\"\nexit {clipboard_exit}\n"),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::write(workspace.join("trace.log"), format!("{EARLIER_LINE}\n")).unwrap();
    let server = FakeServer::start([]);
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
    .unwrap();
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
    let session = PtySession::spawn(command, 24, 400).unwrap();
    session
        .wait_for(WAIT, |screen| screen.contains("Run /help for commands"))
        .unwrap();
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
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1, "{entries:?}");
    entries[0].clone()
}

fn quit(launched: &mut Launched) {
    launched.session.send(b"/quit\r");
    assert!(launched.session.wait_exit(WAIT).unwrap().success());
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
