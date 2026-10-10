use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use ofx_testkit::{FakeServer, Reply, chat_text_events};
use serde_json::{Value, json};

const GATEWAY_UNAVAILABLE: &str = "the gateway provider is not available in oh-fx yet; add a connection under \"providers\" in ~/.config/oh-fx/settings.json and select it with \"provider\" or OH_FX_PROVIDER";
const MCP_EMPTY_TEXT: &str = "[doctor] mcp_connection_check=not_checked\n[doctor] mcp_servers=0 mcp_configuration_issues=0\n";
const MCP_EMPTY_JSON: &str = r#""mcp":{"connection_check":"not_checked","servers":[],"configuration_issues":[],"inspection_error":null}"#;

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = fs::canonicalize(directory.path()).expect("canonicalize the home");
        for name in ["workspace", "bin", "config/oh-fx"] {
            fs::create_dir_all(root.join(name)).expect("create a directory");
        }
        Self {
            _directory: directory,
            root,
        }
    }

    fn workspace(&self) -> String {
        self.root.join("workspace").display().to_string()
    }

    fn write_settings(&self, settings: &Value) {
        fs::write(
            self.root.join("config/oh-fx/settings.json"),
            settings.to_string(),
        )
        .expect("write settings");
    }

    fn install_gh(&self) {
        let gh = self.root.join("bin/gh");
        fs::write(&gh, "#!/bin/sh\n").expect("write gh");
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("make gh executable");
    }

    fn sign_in_to_codex(&self, expires_at_ms: i64) {
        let data = self.root.join("data/oh-fx");
        fs::create_dir_all(&data).expect("create the data directory");
        fs::set_permissions(&data, fs::Permissions::from_mode(0o700))
            .expect("make the data directory private");
        let file = data.join("chatgpt-auth.json");
        let session = json!({
            "version": 1,
            "access_token": "eyJhbGciOiJub25lIn0.c2F2ZWQtYWNjZXNz.c2lnbmF0dXJl",
            "refresh_token": "rt-refresh-secret-0123456789",
            "expires_at_ms": expires_at_ms,
            "account_id": "acct_test",
        });
        fs::write(&file, format!("{session}\n")).expect("write credentials");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
            .expect("make the credentials private");
    }

    fn run(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
            .current_dir(self.root.join("workspace"))
            .env_clear()
            .env("HOME", &self.root)
            .env("PATH", self.root.join("bin"))
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

    fn doctor(&self, args: &[&str], environment: &[(&str, &str)]) -> String {
        let mut full = vec!["doctor"];
        full.extend_from_slice(args);
        let output = self.run(&full, environment);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            text(&output.stderr)
        );
        assert_eq!(text(&output.stderr), "", "{args:?}");
        text(&output.stdout)
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn local_settings(base_url: &str) -> Value {
    json!({
        "provider": "local",
        "providers": {"local": {
            "protocol": "openai-chat-completions",
            "base_url": base_url,
            "auth": {"type": "bearer", "env": "LOCAL_KEY"},
        }},
        "models": {"local": "model-a"},
    })
}

#[test]
fn a_fresh_profile_reports_what_is_missing() {
    let home = Home::new();
    let workspace = home.workspace();
    let stdout = home.doctor(&[], &[]);
    assert_eq!(
        stdout,
        format!(
            "[doctor] ok=1 warn=5 fail=2\n[doctor] workspace={workspace}\n[doctor] model=\n[doctor] auth=missing\n[doctor] auth_refreshable=false\n[doctor] permission_mode=auto\n[doctor] agent_step_limit=0\n{MCP_EMPTY_TEXT}[ok] workspace: using workspace {workspace}\n[warn] config: no config files found; using defaults and env overrides\n[fail] auth: {GATEWAY_UNAVAILABLE}\n[fail] startup: {GATEWAY_UNAVAILABLE}\n[warn] state: durable state is not initialized\n[warn] sessions: no saved sessions yet\n[warn] git: not a git repository; pr/issue workflows will be limited\n[warn] gh: GitHub CLI not found in PATH; publish workflows unavailable\n"
        )
    );
    let detail = GATEWAY_UNAVAILABLE.replace('"', "\\\"");
    assert_eq!(
        home.doctor(&["--json"], &[]),
        format!(
            r#"{{"kind":"doctor","ok_count":1,"warn_count":5,"fail_count":2,"workspace":"{workspace}","model":"","auth":"missing","auth_refreshable":false,"permission_mode":"auto","agent_step_limit":0,"checks":[{{"name":"workspace","status":"ok","detail":"using workspace {workspace}"}},{{"name":"config","status":"warn","detail":"no config files found; using defaults and env overrides"}},{{"name":"auth","status":"fail","detail":"{detail}"}},{{"name":"startup","status":"fail","detail":"{detail}"}},{{"name":"state","status":"warn","detail":"durable state is not initialized"}},{{"name":"sessions","status":"warn","detail":"no saved sessions yet"}},{{"name":"git","status":"warn","detail":"not a git repository; pr/issue workflows will be limited"}},{{"name":"gh","status":"warn","detail":"GitHub CLI not found in PATH; publish workflows unavailable"}}],{MCP_EMPTY_JSON}}}
"#
        )
    );
}

#[test]
fn a_ready_workspace_passes_every_check() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["done"]))]);
    let home = Home::new();
    home.write_settings(&local_settings(&server.base_url()));
    home.install_gh();
    fs::create_dir_all(home.root.join("workspace/.git")).expect("create git metadata");
    fs::write(home.root.join("workspace/.oh-fx.json"), r#"{"model":"x"}"#)
        .expect("write the project settings");
    let environment = [("LOCAL_KEY", "secret")];
    let output = home.run(&["ask", "hello"], &environment);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let id = fs::read_dir(home.root.join("data/oh-fx/sessions"))
        .expect("the sessions directory")
        .next()
        .expect("a session")
        .expect("a session entry")
        .file_name()
        .into_string()
        .expect("a UTF-8 id");
    let workspace = home.workspace();
    let sessions = home.root.join("data/oh-fx/sessions").display().to_string();
    assert_eq!(
        home.doctor(&[], &environment),
        format!(
            "[doctor] ok=8 warn=1 fail=0\n[doctor] workspace={workspace}\n[doctor] model=model-a\n[doctor] model_source=local\n[doctor] auth=configured provider\n[doctor] auth_refreshable=false\n[doctor] permission_mode=auto\n[doctor] agent_step_limit=0\n{MCP_EMPTY_TEXT}[ok] workspace: using workspace {workspace}\n[ok] config: loaded config from ~/.config/oh-fx/settings.json, .oh-fx.json\n[warn] config: project config diagnostic: ignored_project_user_only_setting; key=model\n[ok] auth: configured provider is configured; refreshable=false\n[ok] startup: resolved model=model-a, permission_mode=auto, agent_step_limit=0\n[ok] state: state dir ready at {sessions} (per-session managed state created on demand)\n[ok] sessions: 1 saved session(s); latest={id}\n[ok] git: git metadata detected for this workspace\n[ok] gh: GitHub CLI found in PATH\n"
        )
    );
    let json: Value =
        serde_json::from_str(&home.doctor(&["--json"], &environment)).expect("doctor JSON");
    assert_eq!(json["model_source"], "local");
    assert_eq!(json["ok_count"], 8);
}

#[test]
fn credential_problems_are_reported_as_auth_checks() {
    let home = Home::new();
    home.write_settings(&local_settings("http://127.0.0.1:9/v1"));
    let stdout = home.doctor(&[], &[]);
    assert!(
        stdout.contains("\n[fail] auth: The configured provider credential is unavailable. Check its auth environment variable in settings.json; no other provider was selected.\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("\n[ok] config: loaded config from ~/.config/oh-fx/settings.json\n"),
        "{stdout}"
    );

    let codex = Home::new();
    codex.write_settings(&json!({"provider": "codex", "models": {"codex": "gpt-5.4"}}));
    codex.sign_in_to_codex(1);
    let stdout = codex.doctor(&[], &[("OH_FX_PERMISSION_MODE", "yolo")]);
    assert!(
        stdout.contains("\n[doctor] model_source=Codex subscription\n[doctor] auth=Codex subscription\n[doctor] auth_refreshable=true\n[doctor] auth_expired=true\n[doctor] permission_mode=full access\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("\n[warn] auth: Codex subscription is configured; session expired; refreshable=true\n[ok] startup: resolved model=gpt-5.4, permission_mode=full access, agent_step_limit=0\n"),
        "{stdout}"
    );
    let json = codex.doctor(&["--json"], &[("OH_FX_PERMISSION_MODE", "yolo")]);
    assert!(
        json.contains(r#","auth":"Codex subscription","auth_refreshable":true,"auth_expired":true,"permission_mode":"yolo","#),
        "{json}"
    );
}

#[test]
fn configuration_and_mcp_problems_become_checks() {
    let home = Home::new();
    home.write_settings(&json!({"provider": "codex"}));
    fs::write(
        home.root.join("config/oh-fx/mcp.json"),
        r#"{"mcp":{"docs":{"command":"node"}},"mcpServers":{"x":{"command":"y"}}}"#,
    )
    .expect("write mcp.json");
    let stdout = home.doctor(&[], &[]);
    assert!(
        stdout.contains("\n[warn] mcp_config: ~/.config/oh-fx/mcp.json warning: ignored_mcp_servers_alias key=mcpServers additional_matches=0\n[fail] auth: oh-fx needs a Codex subscription login for this model. Run oh-fx login codex.\n[fail] startup: no Codex model is selected; run `oh-fx provider codex` to choose one, or set a model for this run with --model or OH_FX_MODEL\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("\n[doctor] mcp_servers=1 mcp_configuration_issues=0\n[doctor] mcp_server=docs source=profile scope=profile admission=not_applicable transport=stdio connection=not_checked authentication=not_checked\n"),
        "{stdout}"
    );

    fs::write(home.root.join("config/oh-fx/mcp.json"), "{").expect("write mcp.json");
    let stdout = home.doctor(&[], &[]);
    assert!(
        stdout.contains(
            "\n[fail] mcp_config: failed to load ~/.config/oh-fx/mcp.json: McpConfigInvalidJson\n"
        ),
        "{stdout}"
    );

    home.write_settings(&json!({"provider": "bogus provider"}));
    let stdout = home.doctor(&[], &[]);
    assert!(
        stdout.contains("\n[fail] config: failed to load config: InvalidProviderValue\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "\n[fail] startup: failed to resolve startup settings: InvalidProviderValue\n"
        ),
        "{stdout}"
    );
    assert!(stdout.contains("\n[doctor] model=\n"), "{stdout}");
}

#[test]
fn invalid_additional_directories_leave_the_user_settings_loaded() {
    let home = Home::new();
    let mut settings = local_settings("http://127.0.0.1:9/v1");
    settings["additional_directories"] = json!(["/tmp"]);
    home.write_settings(&settings);
    let stdout = home.doctor(&[], &[("LOCAL_KEY", "secret")]);
    assert!(
        stdout.contains("\n[ok] config: loaded config from ~/.config/oh-fx/settings.json\n[warn] config: user config diagnostic: invalid_additional_directories; key=additional_directories; additional_directories must be an array of at most 16 unique absolute directory paths for the current primary workspace\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "\n[ok] startup: resolved model=model-a, permission_mode=auto, agent_step_limit=0\n"
        ),
        "{stdout}"
    );
}

#[test]
fn a_connection_that_ask_cannot_resolve_fails_the_auth_check() {
    let home = Home::new();
    let mut settings = local_settings("http://127.0.0.1:9/v1");
    home.write_settings(&settings);
    let bad_token = [("LOCAL_KEY", "bad token")];
    let refused = "the configured provider credential is not a valid bearer token; it must be at most 16 KiB of visible ASCII characters";
    let output = home.run(&["ask", "hello"], &bad_token);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output.stderr).contains(refused),
        "{}",
        text(&output.stderr)
    );
    let stdout = home.doctor(&[], &bad_token);
    assert!(
        stdout.contains(&format!("\n[fail] auth: {refused}\n")),
        "{stdout}"
    );
    assert!(
        stdout.starts_with("[doctor] ok=3 warn=4 fail=1\n"),
        "{stdout}"
    );

    settings["providers"]["local"]["headers"] = json!({"x-team": "${TEAM_ID}"});
    home.write_settings(&settings);
    let key = [("LOCAL_KEY", "secret")];
    let missing = "header x-team needs the environment variable TEAM_ID, which is not set; export it or give a default with ${TEAM_ID:-value}";
    let output = home.run(&["ask", "hello"], &key);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output.stderr).contains(missing),
        "{}",
        text(&output.stderr)
    );
    let stdout = home.doctor(&[], &key);
    assert!(
        stdout.contains(&format!("\n[fail] auth: {missing}\n")),
        "{stdout}"
    );
}

#[test]
fn an_unusable_profile_fails_the_startup_check_as_ask_refuses_it() {
    let home = Home::new();
    fs::write(home.root.join("config/oh-fx/settings.json"), "{").expect("write settings");
    let environment = [
        ("OH_FX_PROVIDER", "codex"),
        ("OH_FX_MODEL", "gpt-5.4"),
        ("OH_FX_AUTH_MODE", "host-managed"),
    ];
    let output = home.run(&["ask", "hello"], &environment);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stderr), "oh-fx: InvalidProfileConfiguration\n");
    let stdout = home.doctor(&[], &environment);
    assert!(
        stdout.contains("\n[fail] startup: InvalidProfileConfiguration\n"),
        "{stdout}"
    );
    assert!(stdout.contains("\n[doctor] model=\n"), "{stdout}");
}

#[test]
fn doctor_refuses_the_v2_store() {
    let home = Home::new();
    let output = home.run(&["doctor"], &[("OH_FX_SESSIONS_V2", "1")]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stderr), "oh-fx: doctor is not available yet\n");
}

fn break_session(sessions: &Path, id: &str) {
    let session = sessions.join(id);
    fs::create_dir_all(&session).expect("create a session directory");
    fs::write(session.join("session.json"), "{").expect("write a broken session");
}

fn session_checks(report: &Value) -> Vec<(String, String)> {
    let mut checks: Vec<(String, String)> = report["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .filter(|check| check["name"] == "session")
        .map(|check| {
            (
                check["status"].as_str().expect("a status").to_owned(),
                check["detail"].as_str().expect("a detail").to_owned(),
            )
        })
        .collect();
    checks.sort();
    checks
}

#[test]
fn damaged_sessions_are_reported_with_upstreams_recovery_guidance() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["done"]))]);
    let home = Home::new();
    home.write_settings(&local_settings(&server.base_url()));
    let environment = [("LOCAL_KEY", "secret")];
    let output = home.run(&["ask", "hello"], &environment);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let sessions = home.root.join("data/oh-fx/sessions");
    let id = fs::read_dir(&sessions)
        .expect("the sessions directory")
        .next()
        .expect("a session")
        .expect("a session entry")
        .file_name()
        .into_string()
        .expect("a UTF-8 id");
    let elsewhere = home.root.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("create a directory");
    symlink(&elsewhere, sessions.join(&id).join("tool-results")).expect("link tool results");
    break_session(&sessions, "broken-session");
    let pending = sessions.join("pending");
    fs::create_dir_all(&pending).expect("create a session directory");
    fs::write(pending.join("authority.pending.json"), "{}").expect("write a fence");

    let shown = sessions.display();
    let report: Value =
        serde_json::from_str(&home.doctor(&["--json"], &environment)).expect("doctor JSON");
    let mut expected = vec![
        (
            "fail".to_owned(),
            format!("session {id}: unsafe_path; recovery=back up {shown} and avoid opening this session until the path is repaired"),
        ),
        (
            "fail".to_owned(),
            format!("session broken-session: canonical_state_invalid; recovery=back up {shown}, then inspect this session with oh-fx session broken-session --json"),
        ),
        (
            "warn".to_owned(),
            "session pending: authority_transition_pending report_only=true; recovery=rerun oh-fx doctor after active writers exit; cleanup is guarded".to_owned(),
        ),
    ];
    expected.sort();
    assert_eq!(session_checks(&report), expected);
    let names: Vec<&str> = report["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .map(|check| check["name"].as_str().expect("a name"))
        .collect();
    let state = names
        .iter()
        .position(|name| *name == "state")
        .expect("a state check");
    assert_eq!(
        names[state + 1..state + 5],
        ["session", "session", "session", "sessions"]
    );
    let text = home.doctor(&[], &environment);
    assert!(
        text.contains(&format!("\n[fail] session: session broken-session: canonical_state_invalid; recovery=back up {shown}, then inspect this session with oh-fx session broken-session --json\n")),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "\n[ok] sessions: 1 saved session(s); latest={id}\n"
        )),
        "{text}"
    );
}

#[test]
fn session_diagnostics_stop_after_sixty_four_session_directories() {
    let home = Home::new();
    let sessions = home.root.join("data/oh-fx/sessions");
    fs::create_dir_all(&sessions).expect("create the sessions directory");
    for directory in [home.root.join("data/oh-fx"), sessions.clone()] {
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("make the directory private");
    }
    for index in 0..65 {
        break_session(&sessions, &format!("broken-{index:02}"));
    }
    let report: Value = serde_json::from_str(&home.doctor(&["--json"], &[])).expect("doctor JSON");
    let checks = session_checks(&report);
    assert_eq!(checks.len(), 65);
    let invalid = checks
        .iter()
        .filter(|(status, detail)| {
            status == "fail" && detail.contains(": canonical_state_invalid; ")
        })
        .count();
    assert_eq!(invalid, 64);
    assert!(
        checks.contains(&(
            "warn".to_owned(),
            "session diagnostics truncated after 64 session directories to keep doctor bounded"
                .to_owned()
        ))
    );
}

#[test]
fn a_session_folder_open_to_others_or_a_linked_log_is_reported_unsafe() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["one"])),
        Reply::sse(&chat_text_events(&["two"])),
    ]);
    let home = Home::new();
    home.write_settings(&local_settings(&server.base_url()));
    let environment = [("LOCAL_KEY", "secret")];
    for prompt in ["first", "second"] {
        let output = home.run(&["ask", prompt], &environment);
        assert!(output.status.success(), "{}", text(&output.stderr));
    }
    let sessions = home.root.join("data/oh-fx/sessions");
    let mut ids: Vec<String> = fs::read_dir(&sessions)
        .expect("the sessions directory")
        .map(|entry| {
            entry
                .expect("a session entry")
                .file_name()
                .into_string()
                .expect("a UTF-8 id")
        })
        .collect();
    ids.sort();
    let [open, linked] = ids.as_slice() else {
        panic!("two sessions: {ids:?}");
    };
    fs::set_permissions(sessions.join(open), fs::Permissions::from_mode(0o755))
        .expect("open the session folder");
    let log = sessions.join(linked).join("events.jsonl");
    let moved = home.root.join("events.jsonl");
    fs::rename(&log, &moved).expect("move the log");
    symlink(&moved, &log).expect("link the log");

    let shown = sessions.display();
    let report: Value =
        serde_json::from_str(&home.doctor(&["--json"], &environment)).expect("doctor JSON");
    assert_eq!(
        session_checks(&report),
        [open, linked].map(|id| (
            "fail".to_owned(),
            format!("session {id}: unsafe_path; recovery=back up {shown} and avoid opening this session until the path is repaired"),
        ))
    );
    let shown_session = home.run(&["session", linked], &environment);
    assert_eq!(shown_session.status.code(), Some(1));
    assert_eq!(
        text(&shown_session.stderr),
        "oh-fx session: record not found\n"
    );
}
