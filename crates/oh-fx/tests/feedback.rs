use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use ofx_testkit::{FakeServer, PtySession};
use serde_json::json;

const URL: &str = "https://github.com/binbandit/oh-fx/issues/new";
const WAIT: Duration = Duration::from_secs(15);
const HELD_LAUNCHER_POLLS: u32 = 1200;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Launch {
    Exit(i32),
    Signal,
    Held,
}

#[test]
fn feedback_routes_to_the_existing_opener_and_reports_launcher_outcomes() {
    for launch in [
        Some(Launch::Exit(0)),
        Some(Launch::Exit(7)),
        Some(Launch::Signal),
        Some(Launch::Held),
        None,
    ] {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let config = root.join("config/oh-fx");
        let bin = root.join("bin");
        for path in [&workspace, &config, &bin] {
            fs::create_dir_all(path).unwrap();
        }
        let captured = root.join("opened-url");
        let captured_pid = root.join("launcher-pid");
        let hold = root.join("launcher-hold");
        if let Some(launch) = launch {
            install_launcher(&bin, launch);
        }
        if launch == Some(Launch::Held) {
            fs::write(&hold, "").unwrap();
        }
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .current_dir(&workspace)
            .env_clear()
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("PATH", &bin)
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .env("OH_FX_FEEDBACK_TEST_URL", &captured)
            .env("OH_FX_FEEDBACK_TEST_PID", &captured_pid)
            .env("OH_FX_FEEDBACK_TEST_HOLD", &hold)
            .process_group(0);
        let mut session = PtySession::spawn(command, 24, 120).unwrap();
        session
            .wait_for(WAIT, |screen| screen.contains("Run /help for commands"))
            .unwrap();
        session.send(b"/feedback\r");
        if launch == Some(Launch::Held) {
            session.send(b"/version\r");
        }
        let notice = if matches!(launch, Some(Launch::Exit(0) | Launch::Held)) {
            format!("Opened {URL}.")
        } else {
            format!("Could not open {URL}. Open it manually.")
        };
        let screen = session
            .wait_for(WAIT, |screen| screen.contains(&notice))
            .unwrap();
        if launch == Some(Launch::Held) {
            assert_reported_after_version(&screen, &notice);
        }
        if launch.is_some() {
            session
                .wait_for(WAIT, |_| {
                    fs::read_to_string(&captured).is_ok_and(|value| value == URL)
                })
                .unwrap();
        }
        if launch == Some(Launch::Held) {
            release_held_launcher(&session, &captured_pid, &hold);
        }
        session.send(b"/quit\r");
        assert!(session.wait_exit(WAIT).unwrap().success());
        if launch.is_some() {
            assert_eq!(fs::read_to_string(&captured).unwrap(), URL);
        } else {
            assert!(!captured.exists());
        }
        assert!(server.requests().is_empty());
    }
}

fn assert_reported_after_version(screen: &str, notice: &str) {
    let version = screen.find(&format!("version: {}", ofx_upgrade::VERSION));
    let reported = screen.find(notice);
    assert!(
        version
            .zip(reported)
            .is_some_and(|(version, reported)| version < reported),
        "{screen}"
    );
}

fn release_held_launcher(session: &PtySession, captured_pid: &Path, hold: &Path) {
    session
        .wait_for(WAIT, |_| {
            fs::read_to_string(captured_pid).is_ok_and(|pid| !pid.is_empty())
        })
        .expect("the held launcher records its process id");
    let pid = fs::read_to_string(captured_pid).expect("read the held launcher's process id");
    assert!(launcher_running(&pid));
    fs::remove_file(hold).expect("release the held launcher");
    session
        .wait_for(WAIT, |_| !launcher_running(&pid))
        .expect("the released launcher is reaped");
}

fn launcher_running(pid: &str) -> bool {
    Command::new("/bin/kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("check whether the launcher remains alive")
        .success()
}

fn install_launcher(bin: &Path, launch: Launch) {
    let launcher = bin.join(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    });
    let outcome = match launch {
        Launch::Signal => "kill -TERM $$".to_owned(),
        Launch::Held => format!(
            "polls=0\nwhile [ -e \"$OH_FX_FEEDBACK_TEST_HOLD\" ] && [ \"$polls\" -lt {HELD_LAUNCHER_POLLS} ]; do\n/bin/sleep 0.1\npolls=$((polls + 1))\ndone\nexit 0"
        ),
        Launch::Exit(status) => format!("exit {status}"),
    };
    fs::write(&launcher, format!("#!/bin/sh\nprintf '%s' \"$1\" > \"$OH_FX_FEEDBACK_TEST_URL\"\nprintf '%s' \"$$\" > \"$OH_FX_FEEDBACK_TEST_PID\"\n{outcome}\n")).expect("install the test launcher");
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700))
        .expect("install the test launcher");
}
