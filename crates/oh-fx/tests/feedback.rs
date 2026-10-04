use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Duration;

use ofx_testkit::{FakeServer, PtySession};
use serde_json::json;

const URL: &str = "https://github.com/binbandit/oh-fx/issues/new";
const WAIT: Duration = Duration::from_secs(15);

#[test]
fn feedback_routes_to_the_existing_opener_and_reports_spawn_outcomes() {
    for launch in [Some(0), Some(7), None] {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let config = root.join("config/oh-fx");
        let bin = root.join("bin");
        for path in [&workspace, &config, &bin] {
            fs::create_dir_all(path).unwrap();
        }
        let captured = root.join("opened-url");
        if let Some(status) = launch {
            let launcher = bin.join(if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            });
            fs::write(
                &launcher,
                format!(
                    "#!/bin/sh\nprintf '%s' \"$1\" > \"$OH_FX_FEEDBACK_TEST_URL\"\nexit {status}\n"
                ),
            )
            .unwrap();
            fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
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
            .process_group(0);
        let mut session = PtySession::spawn(command, 24, 120).unwrap();
        session
            .wait_for(WAIT, |screen| screen.contains("Run /help for commands"))
            .unwrap();
        session.send(b"/feedback\r");
        let notice = if launch.is_some() {
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
