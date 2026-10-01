use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const LOGIN_HELP: &str = include_str!("../../ofx-cli/tests/golden/command_login.txt");
const LOGOUT_HELP: &str = include_str!("../../ofx-cli/tests/golden/command_logout.txt");

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory.path().to_owned();
        Self {
            _directory: directory,
            root,
        }
    }

    fn data(&self) -> PathBuf {
        self.root.join("data/oh-fx")
    }

    fn credential_file(&self) -> PathBuf {
        self.data().join("chatgpt-auth.json")
    }

    fn write_credentials(&self) {
        fs::create_dir_all(self.data()).expect("create the data directory");
        fs::set_permissions(self.data(), fs::Permissions::from_mode(0o700))
            .expect("make the data directory private");
        fs::write(
            self.credential_file(),
            "{\"version\":1,\"access_token\":\"a.b.c\",\"refresh_token\":\"refresh\",\"expires_at_ms\":1,\"account_id\":\"acct\"}\n",
        )
        .expect("write credentials");
        fs::set_permissions(self.credential_file(), fs::Permissions::from_mode(0o600))
            .expect("make credentials private");
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[])
    }

    fn run_with(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("OH_FX_AUTO_UPGRADE", "0")
            .env("OH_FX_NO_OPEN_BROWSER", "1")
            .envs(environment.iter().copied())
            .stdin(Stdio::null())
            .output()
            .expect("run oh-fx")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn logout_codex_removes_the_saved_login() {
    let home = Home::new();
    home.write_credentials();
    let output = home.run(&["logout", "codex"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Signed out of Codex.\n");
    assert!(!home.credential_file().exists());

    let again = home.run(&["logout", "codex"]);
    assert!(again.status.success());
    assert_eq!(stdout(&again), "No Codex login session found.\n");
}

#[test]
fn login_rejects_unknown_providers_with_usage() {
    let home = Home::new();
    let output = home.run(&["login", "chatgpt"]);
    assert!(!output.status.success());
    assert_eq!(stderr(&output), "usage: oh-fx login [vercel|codex|grok]\n");
    let output = home.run(&["logout", "codex", "grok"]);
    assert_eq!(stderr(&output), "usage: oh-fx logout [vercel|codex|grok]\n");
}

#[test]
fn other_provider_logins_are_not_available_yet() {
    let home = Home::new();
    for (args, command) in [
        (&["login"][..], "login"),
        (&["login", "vercel"], "login"),
        (&["logout", "grok"], "logout"),
    ] {
        let output = home.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stdout(&output), "", "{args:?}");
        assert_eq!(
            stderr(&output),
            format!("oh-fx: {command} is not available yet\n"),
            "{args:?}"
        );
    }
}

#[test]
fn host_managed_authentication_skips_sign_in_and_sign_out() {
    let home = Home::new();
    home.write_credentials();
    for args in [&["login", "codex"][..], &["logout", "codex"], &["login"]] {
        let output = home.run_with(args, &[("OH_FX_AUTH_MODE", "host-managed")]);
        assert!(output.status.success(), "{args:?}");
        assert_eq!(stdout(&output), "Authentication is managed by the host.\n");
        assert_eq!(stderr(&output), "");
    }
    assert!(home.credential_file().exists());
}

#[test]
fn login_refuses_unwritable_credential_storage_before_listening() {
    let home = Home::new();
    let data = home.root.join("data");
    fs::create_dir_all(&data).expect("create the data root");
    fs::set_permissions(&data, fs::Permissions::from_mode(0o500)).expect("make it read-only");
    let output = home.run(&["login", "codex"]);
    fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).expect("restore access");
    assert!(!output.status.success());
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        "oh-fx login: Codex subscription: Saved credential storage is unavailable. Check the saved credential, then retry.\n"
    );
}

#[test]
fn login_and_logout_print_their_golden_help() {
    let home = Home::new();
    for (args, golden) in [
        (&["login", "--help"][..], LOGIN_HELP),
        (&["login", "codex", "-h"], LOGIN_HELP),
        (&["logout", "-h"], LOGOUT_HELP),
    ] {
        let output = home.run(args);
        assert!(output.status.success(), "{args:?}");
        assert_eq!(stdout(&output), golden, "{args:?}");
    }
}

#[test]
fn codex_models_need_a_selected_model_and_a_login() {
    let home = Home::new();
    let unselected = home.run_with(&["models"], &[("OH_FX_PROVIDER", "codex")]);
    assert_eq!(unselected.status.code(), Some(1));
    assert_eq!(stdout(&unselected), "");
    assert!(
        stderr(&unselected).starts_with("oh-fx: no Codex model is selected; "),
        "{}",
        stderr(&unselected)
    );

    let environment = [("OH_FX_PROVIDER", "codex"), ("OH_FX_MODEL", "gpt-6.1-sol")];
    let text = home.run_with(&["models"], &environment);
    assert_eq!(text.status.code(), Some(1));
    assert_eq!(stdout(&text), "");
    assert_eq!(
        stderr(&text),
        "oh-fx models: could not list models: AuthenticationRejected\n"
    );
    let json = home.run_with(&["models", "--json"], &environment);
    assert_eq!(json.status.code(), Some(1));
    assert_eq!(stderr(&json), "");
    assert_eq!(
        stdout(&json),
        "{\"kind\":\"models\",\"error\":\"could not list models: AuthenticationRejected\",\"code\":\"AuthenticationRejected\"}\n"
    );
}

#[test]
fn only_codex_can_be_selected_with_the_provider_command() {
    let home = Home::new();
    for name in ["gateway", "grok", "portkey"] {
        let output = home.run(&["provider", name]);
        assert_eq!(output.status.code(), Some(1), "{name}");
        assert_eq!(stdout(&output), "", "{name}");
        assert_eq!(
            stderr(&output),
            "oh-fx: provider is not available yet\n",
            "{name}"
        );
    }
    let settings = home.root.join("config/oh-fx");
    fs::create_dir_all(&settings).expect("create the config directory");
    fs::write(settings.join("settings.json"), "{\"provider\":").expect("write settings");
    let output = home.run(&["provider", "codex"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "oh-fx provider: could not load settings\n");
}
