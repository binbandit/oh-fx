use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Duration;

use ofx_testkit::{FakeServer, PtySession};
use serde_json::json;

const URL: &str = "https://github.com/binbandit/oh-fx/issues/new";
const WAIT: Duration = Duration::from_secs(15);

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
        if let Some(launch) = launch {
            install_launcher(&bin, launch);
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
            .process_group(0);
        let mut session = PtySession::spawn(command, 24, 120).unwrap();
        session
            .wait_for(WAIT, |screen| screen.contains("Run /help for commands"))
            .unwrap();
        session.send(b"/feedback\r");
        if launch == Some(Launch::Held) {
            session.send(b"/version\r");
            let screen = session
                .wait_for(Duration::from_secs(1), |screen| {
                    screen.contains(&format!("version: {}", ofx_upgrade::VERSION))
                })
                .unwrap();
            assert!(!screen.contains(&format!("Opened {URL}.")), "{screen}");
        }
        let notice = if matches!(launch, Some(Launch::Exit(0) | Launch::Held)) {
            format!("Opened {URL}.")
        } else {
            format!("Could not open {URL}. Open it manually.")
        };
        session
            .wait_for(WAIT, |screen| screen.contains(&notice))
            .unwrap();
        if launch.is_some() {
            session
                .wait_for(WAIT, |_| {
                    fs::read_to_string(&captured).is_ok_and(|value| value == URL)
                })
                .unwrap();
        }
        if launch == Some(Launch::Held) {
            let pid = fs::read_to_string(&captured_pid).unwrap();
            assert!(launcher_running(&pid));
            session.wait_for(WAIT, |_| !launcher_running(&pid)).unwrap();
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

fn launcher_running(pid: &str) -> bool {
    Command::new("/bin/kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("check whether the launcher remains alive")
        .success()
}

fn install_launcher(bin: &std::path::Path, launch: Launch) {
    let launcher = bin.join(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    });
    let outcome = match launch {
        Launch::Signal => "kill -TERM $$".to_owned(),
        Launch::Held => "/bin/sleep 4\nexit 0".to_owned(),
        Launch::Exit(status) => format!("exit {status}"),
    };
    fs::write(&launcher, format!("#!/bin/sh\nprintf '%s' \"$1\" > \"$OH_FX_FEEDBACK_TEST_URL\"\nprintf '%s' \"$$\" > \"$OH_FX_FEEDBACK_TEST_PID\"\n{outcome}\n")).expect("install the test launcher");
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700))
        .expect("install the test launcher");
}
