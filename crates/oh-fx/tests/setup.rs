use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use ofx_testkit::PtySession;

const WAIT: Duration = Duration::from_secs(15);
const PROMPT: &str = "Paste AI Gateway API key (input hidden): ";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = fs::canonicalize(directory.path()).expect("canonicalize the home");
        fs::create_dir_all(root.join("workspace")).expect("create the workspace");
        Self {
            _directory: directory,
            root,
        }
    }

    fn command(&self, args: &[&str], environment: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .args(args)
            .current_dir(self.root.join("workspace"))
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .envs(environment.iter().copied());
        command
    }

    fn run(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        self.command(args, environment)
            .stdin(Stdio::null())
            .output()
            .expect("run oh-fx")
    }

    fn setup_in_terminal(&self) -> PtySession {
        let session = PtySession::spawn(self.command(&["setup"], &[]), 24, 100)
            .expect("spawn oh-fx setup in a pty");
        session
            .wait_for(WAIT, |screen| screen.contains(PROMPT.trim_end()))
            .unwrap_or_else(|screen| panic!("the paste prompt appears:\n{screen}"));
        session
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn host_managed_authentication_needs_no_stored_key() {
    let home = Home::new();
    let output = home.run(&["setup"], &[("OH_FX_AUTH_MODE", "host-managed")]);
    assert!(output.status.success());
    assert_eq!(
        text(&output.stdout),
        "Authentication is managed by the host.\n"
    );
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn a_disabled_store_or_a_missing_terminal_saves_nothing() {
    let home = Home::new();
    let output = home.run(&["setup"], &[("OH_FX_DISABLE_KEYCHAIN", "1")]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "oh-fx setup: stored API keys are disabled by OH_FX_DISABLE_KEYCHAIN\n"
    );
    let output = home.run(&["setup"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "oh-fx setup: an interactive terminal is required to paste an API key\n"
    );
    assert!(!home.root.join("data").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn without_an_account_name_the_keychain_is_never_asked() {
    let home = Home::new();
    let mut session = home.setup_in_terminal();
    let screen = session
        .wait_for(WAIT, |screen| screen.contains("API key was not saved"))
        .unwrap_or_else(|screen| panic!("the save fails:\n{screen}"));
    let rows: Vec<&str> = screen.lines().map(str::trim_end).collect();
    assert!(
        rows.windows(2)
            .any(|pair| pair == [PROMPT.trim_end(), "oh-fx setup: API key was not saved"]),
        "{screen}"
    );
    assert_eq!(
        session.wait_exit(WAIT).and_then(|status| status.code()),
        Some(1)
    );
}

#[cfg(not(target_os = "macos"))]
mod profile_file {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use ofx_testkit::{FakeServer, Reply};
    use serde_json::json;

    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    fn stored_key_file(home: &Home) -> PathBuf {
        home.root.join("data/oh-fx/api-key")
    }

    #[test]
    fn a_pasted_key_is_masked_saved_privately_and_used_by_the_gateway() {
        let home = Home::new();
        let mut session = home.setup_in_terminal();
        session.send(b"vck-abcX\x7fd\r");
        let screen = session
            .wait_for(WAIT, |screen| screen.contains("Saved API key"))
            .unwrap_or_else(|screen| panic!("the key is saved:\n{screen}"));
        assert!(
            screen.contains("Saved API key to profile file."),
            "{screen}"
        );
        assert!(!screen.contains("vck-abc"), "{screen}");
        assert!(
            session
                .wait_exit(WAIT)
                .is_some_and(|status| status.success())
        );
        let file = stored_key_file(&home);
        assert_eq!(fs::read_to_string(&file).unwrap(), "vck-abcd");
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(file.parent().unwrap()), 0o700);

        let status = home.run(&["status"], &[]);
        assert!(
            text(&status.stdout).contains("\n[status] auth=stored API key (profile file)\n"),
            "{}",
            text(&status.stdout)
        );
        let server = FakeServer::start([Reply::sse(&[
            json!({"type": "text-delta", "id": "t", "delta": "stored"}).to_string(),
            r#"{"type":"finish","finishReason":{"unified":"stop"}}"#.to_owned(),
        ])]);
        let url = format!("http://{}/v4/ai/language-model", server.address());
        let output = home.run(
            &["ask", "--no-save", "hello"],
            &[("OH_FX_E2E_GATEWAY_CHAT_URL", &url)],
        );
        assert_eq!(text(&output.stdout), "stored");
        assert_eq!(
            server.requests()[0].header("authorization"),
            Some("Bearer vck-abcd")
        );
    }

    #[test]
    fn a_cancelled_paste_saves_nothing() {
        let home = Home::new();
        let mut session = home.setup_in_terminal();
        session.send(b"vck-partial\x03");
        let screen = session
            .wait_for(WAIT, |screen| screen.contains("API key was not saved"))
            .unwrap_or_else(|screen| panic!("the paste is cancelled:\n{screen}"));
        assert!(!screen.contains("Saved API key"), "{screen}");
        assert_eq!(
            session.wait_exit(WAIT).and_then(|status| status.code()),
            Some(1)
        );
        assert!(!stored_key_file(&home).exists());
    }

    #[test]
    fn an_unsafe_stored_key_is_reported_as_unreadable_and_never_sent() {
        let home = Home::new();
        let file = stored_key_file(&home);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::set_permissions(file.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&file, "vck-exposed").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let output = home.run(&["ask", "--json", "hello"], &[]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            text(&output.stderr),
            "oh-fx ask: oh-fx could not read the stored API key from profile file. A key may be saved but unreadable. Set OH_FX_TRACE_LOG for the failing step, or set AI_GATEWAY_API_KEY.\n"
        );
        assert!(text(&output.stdout).contains(r#""error":"MissingCredentials""#));
        let status = home.run(&["status"], &[]);
        assert!(
            text(&status.stdout).contains("\n[status] auth=missing\n[status] auth_refreshable=false\n[status] auth_help=oh-fx could not read the stored API key from profile file."),
            "{}",
            text(&status.stdout)
        );
    }
}
