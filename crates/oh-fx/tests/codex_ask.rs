use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

const ACCESS_TOKEN: &str = "eyJhbGciOiJub25lIn0.c2F2ZWQtYWNjZXNz.c2lnbmF0dXJl";
const REFRESH_TOKEN: &str = "rt-refresh-secret-0123456789";
const MISSING_LOGIN: &str =
    "oh-fx ask: oh-fx needs a Codex subscription login for this model. Run oh-fx login codex.\n";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn with_settings(settings: Option<&Value>) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory.path().to_owned();
        fs::create_dir_all(root.join("workspace")).expect("create the workspace");
        if let Some(settings) = settings {
            let config = root.join("config/oh-fx");
            fs::create_dir_all(&config).expect("create the config directory");
            fs::write(config.join("settings.json"), settings.to_string())
                .expect("write settings.json");
        }
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

    fn write_credentials(&self, mode: u32, expires_at_ms: i64) -> String {
        fs::create_dir_all(self.data()).expect("create the data directory");
        fs::set_permissions(self.data(), fs::Permissions::from_mode(0o700))
            .expect("make the data directory private");
        let session = format!(
            "{}\n",
            json!({
                "version": 1,
                "access_token": ACCESS_TOKEN,
                "refresh_token": REFRESH_TOKEN,
                "expires_at_ms": expires_at_ms,
                "account_id": "acct_test",
            })
        );
        fs::write(self.credential_file(), &session).expect("write credentials");
        fs::set_permissions(self.credential_file(), fs::Permissions::from_mode(mode))
            .expect("set the credential mode");
        session
    }

    fn ask<S: AsRef<OsStr>>(&self, args: &[S], environment: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
            .current_dir(self.root.join("workspace"))
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .envs(environment.iter().copied())
            .stdin(Stdio::null())
            .output()
            .expect("run oh-fx")
    }
}

fn codex_settings() -> Value {
    json!({"provider": "codex", "models": {"codex": "gpt-5.4"}})
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn error_json(code: &str) -> String {
    format!(
        "{{\"output\":\"\",\"final_output\":\"\",\"exit_code\":1,\"model\":\"\",\"resolved_provider\":null,\"session_id\":\"\",\"steps\":0,\"tool_calls\":[],\"usage\":{{\"input_tokens\":null,\"output_tokens\":null}},\"error\":\"{code}\"}}\n"
    )
}

#[test]
fn ask_without_a_codex_login_asks_the_user_to_sign_in() {
    let home = Home::with_settings(Some(&codex_settings()));
    let output = home.ask(&["ask", "hello"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), MISSING_LOGIN);

    let json = home.ask(&["ask", "--json", "hello"], &[]);
    assert_eq!(json.status.code(), Some(1));
    assert_eq!(stdout(&json), error_json("MissingCredentials"));
    assert_eq!(stderr(&json), MISSING_LOGIN);
}

#[test]
fn the_environment_selects_codex_and_a_run_model() {
    let home = Home::with_settings(None);
    let unselected = home.ask(&["ask", "hello"], &[("OH_FX_PROVIDER", "codex")]);
    assert_eq!(unselected.status.code(), Some(1));
    assert_eq!(
        stderr(&unselected),
        "oh-fx ask: no Codex model is selected; save one as \"codex\" under \"models\" in ~/.config/oh-fx/settings.json, or set a model for this run with --model or OH_FX_MODEL\n"
    );
    let json = home.ask(&["ask", "--json", "hello"], &[("OH_FX_PROVIDER", "Codex")]);
    assert_eq!(stdout(&json), error_json("CodexModelNotSelected"));

    for (args, environment) in [
        (
            &["ask", "--model", "gpt-5.4", "hello"][..],
            &[("OH_FX_PROVIDER", "codex")][..],
        ),
        (
            &["ask", "hello"][..],
            &[("OH_FX_PROVIDER", "codex"), ("OH_FX_MODEL", "gpt-5.4")][..],
        ),
    ] {
        let output = home.ask(args, environment);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(stderr(&output), MISSING_LOGIN);
    }
}

#[test]
fn ask_refuses_a_codex_login_readable_by_others_without_showing_it() {
    let home = Home::with_settings(Some(&codex_settings()));
    let session = home.write_credentials(0o644, 4_102_444_800_000);
    let output = home.ask(&["ask", "hello"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        "oh-fx ask: Saved credential storage is unavailable. Check the saved credential, then retry.\n"
    );
    let json = home.ask(&["ask", "--json", "hello"], &[]);
    assert_eq!(stdout(&json), error_json("CredentialStorageUnavailable"));
    assert_eq!(stderr(&json), "");
    for shown in [&output, &json].map(|output| format!("{}{}", stdout(output), stderr(output))) {
        assert!(!shown.contains(ACCESS_TOKEN));
        assert!(!shown.contains(REFRESH_TOKEN));
    }
    assert_eq!(
        fs::read_to_string(home.credential_file()).expect("read credentials"),
        session
    );
}

#[test]
fn quiet_codex_runs_still_report_a_missing_login() {
    let home = Home::with_settings(Some(&codex_settings()));
    let output = home.ask(&["ask", "--quiet", "hello"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), MISSING_LOGIN);
}

#[test]
fn non_utf8_codex_models_fail_as_invalid_models_before_an_expired_login_is_refreshed() {
    let home = Home::with_settings(Some(&codex_settings()));
    let session = home.write_credentials(0o600, 1);
    let model = OsString::from_vec(b" m\xff ".to_vec());
    for json in [false, true] {
        let mut args = vec![
            OsString::from("ask"),
            OsString::from("--model"),
            model.clone(),
        ];
        if json {
            args.push(OsString::from("--json"));
        }
        args.push(OsString::from("hello"));
        let output = home.ask(&args, &[]);
        assert_eq!(output.status.code(), Some(1));
        if json {
            assert_eq!(stderr(&output), "");
            assert_eq!(
                stdout(&output),
                "{\"output\":\"\",\"final_output\":\"\",\"exit_code\":1,\"model\":[109,255],\"resolved_provider\":null,\"session_id\":\"\",\"steps\":0,\"tool_calls\":[],\"usage\":{\"input_tokens\":null,\"output_tokens\":null},\"error\":\"InvalidModel\"}\n"
            );
        } else {
            assert_eq!(stdout(&output), "");
            assert_eq!(stderr(&output), "oh-fx: InvalidModel\n");
        }
    }
    assert_eq!(
        fs::read_to_string(home.credential_file()).expect("read credentials"),
        session
    );
}
