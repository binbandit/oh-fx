use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

const MISSING_CODEX: &str =
    "oh-fx needs a Codex subscription login for this model. Run oh-fx login codex.";
const MCP_EMPTY_TEXT: &str = "[status] mcp_connection_check=not_checked\n[status] mcp_servers=0 mcp_configuration_issues=0\n";
const MCP_EMPTY_JSON: &str = r#""mcp":{"connection_check":"not_checked","servers":[],"configuration_issues":[],"inspection_error":null}"#;

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new(settings: Option<&Value>) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = fs::canonicalize(directory.path()).expect("canonicalize the home");
        fs::create_dir_all(root.join("workspace")).expect("create the workspace");
        let home = Self {
            _directory: directory,
            root,
        };
        if let Some(settings) = settings {
            home.write_config("settings.json", &settings.to_string());
        }
        home
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    fn write_config(&self, name: &str, contents: &str) {
        let config = self.root.join("config/oh-fx");
        fs::create_dir_all(&config).expect("create the config directory");
        fs::write(config.join(name), contents).expect("write the config file");
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

    fn status(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        self.status_in(&self.workspace(), args, environment)
    }

    fn status_in(&self, directory: &Path, args: &[&str], environment: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
            .current_dir(directory)
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("OH_FX_AUTO_UPGRADE", "0")
            .envs(environment.iter().copied())
            .stdin(Stdio::null())
            .output()
            .expect("run oh-fx")
    }

    fn succeeds(&self, args: &[&str], environment: &[(&str, &str)]) -> (String, String) {
        let output = self.status(args, environment);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            text(&output.stderr)
        );
        (text(&output.stdout), text(&output.stderr))
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn local_settings(auth: &Value) -> Value {
    json!({
        "provider": "local",
        "providers": {"local": {
            "protocol": "openai-chat-completions",
            "base_url": "http://127.0.0.1:8080/v1",
            "auth": auth,
        }},
        "models": {"local": "model-a"},
    })
}

fn codex_settings() -> Value {
    json!({"provider": "codex", "models": {"codex": "gpt-5.4"}})
}

fn tail(workspace: &str) -> String {
    format!(
        "[status] permission_mode=auto\n[status] workspace={workspace}\n[status] history_turns=0\n[status] session_permission_grants=0\n[status] agent_step_limit=0\n"
    )
}

fn json_tail(workspace: &str) -> String {
    format!(
        r#""permission_mode":"auto","workspace":"{workspace}","history_turns":0,"session_permission_grants":0,"agent_step_limit":0"#
    )
}

#[test]
fn a_configured_connection_reports_its_endpoint_credential_and_mcp_servers() {
    let home = Home::new(Some(&local_settings(
        &json!({"type": "bearer", "env": "LOCAL_KEY"}),
    )));
    home.write_config("mcp.json", r#"{"mcp":{"docs":{"command":"node"}}}"#);
    fs::write(
        home.workspace().join(".mcp.json"),
        r#"{"mcpServers":{"tool":{"command":"x"}}}"#,
    )
    .expect("write the workspace MCP config");
    let workspace = home.workspace().display().to_string();
    let environment = [("LOCAL_KEY", "secret")];

    let (stdout, stderr) = home.succeeds(&["status"], &environment);
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        format!(
            "[status] model=model-a\n[status] model_origin=settings\n[status] model_source=local\n[status] provider_endpoint=http://127.0.0.1:8080/v1\n[status] auth=configured provider\n[status] connected_providers=local\n[status] auth_refreshable=false\n{}[status] mcp_connection_check=not_checked\n[status] mcp_servers=2 mcp_configuration_issues=0\n[status] mcp_server=docs source=profile scope=profile admission=not_applicable transport=stdio connection=not_checked authentication=not_checked\n[status] mcp_server=tool source=workspace scope=workspace admission=pending transport=stdio connection=not_checked authentication=not_checked\n",
            tail(&workspace)
        )
    );

    let (stdout, _) = home.succeeds(&["status", "--json"], &environment);
    assert_eq!(
        stdout,
        format!(
            r#"{{"kind":"status","model":"model-a","model_origin":"settings","model_source":"local","provider_endpoint":"http://127.0.0.1:8080/v1","auth":"configured provider","connected_providers":["local"],"auth_refreshable":false,{},"mcp":{{"connection_check":"not_checked","servers":[{{"name":"docs","source":"profile","scope":"profile","admission":null,"required":false,"transport":"stdio","connection":"not_checked","authentication":"not_checked"}},{{"name":"tool","source":"workspace","scope":"workspace","admission":"pending","required":false,"transport":"stdio","connection":"not_checked","authentication":"not_checked"}}],"configuration_issues":[],"inspection_error":null}}}}
"#,
            json_tail(&workspace)
        )
    );
}

#[test]
fn a_configured_connection_without_its_credential_explains_what_is_missing() {
    let home = Home::new(Some(&local_settings(
        &json!({"type": "bearer", "env": "LOCAL_KEY"}),
    )));
    home.sign_in_to_codex(i64::MAX);
    let workspace = home.workspace().display().to_string();
    let help = "The configured provider credential is unavailable. Check its auth environment variable in settings.json; no other provider was selected.";

    let (stdout, _) = home.succeeds(&["status"], &[]);
    assert_eq!(
        stdout,
        format!(
            "[status] model=model-a\n[status] model_origin=settings\n[status] model_source=local\n[status] provider_endpoint=http://127.0.0.1:8080/v1\n[status] auth=missing\n[status] connected_providers=Codex\n[status] auth_refreshable=false\n[status] auth_help={help}\n{}{MCP_EMPTY_TEXT}",
            tail(&workspace)
        )
    );
    let (stdout, _) = home.succeeds(&["status", "--json"], &[]);
    assert_eq!(
        stdout,
        format!(
            r#"{{"kind":"status","model":"model-a","model_origin":"settings","model_source":"local","provider_endpoint":"http://127.0.0.1:8080/v1","auth":"missing","connected_providers":["codex"],"auth_refreshable":false,"auth_help":"{help}",{},{MCP_EMPTY_JSON}}}
"#,
            json_tail(&workspace)
        )
    );

    let (stdout, _) = home.succeeds(&["status"], &[("LOCAL_KEY", "secret")]);
    assert!(
        stdout.contains("\n[status] connected_providers=local, Codex\n"),
        "{stdout}"
    );
    let (stdout, _) = home.succeeds(&["status", "--json"], &[("LOCAL_KEY", "secret")]);
    assert!(
        stdout.contains(r#","connected_providers":["local","codex"],"#),
        "{stdout}"
    );
}

#[test]
fn a_connection_without_a_saved_model_starts_from_its_first_listed_model() {
    let mut settings = local_settings(&json!({"type": "none"}));
    settings["providers"]["local"]["models"] = json!(["listed-a", "listed-b"]);
    settings
        .as_object_mut()
        .expect("settings object")
        .remove("models");
    let home = Home::new(Some(&settings));
    let (stdout, _) = home.succeeds(&["status"], &[]);
    assert!(
        stdout.starts_with(
            "[status] model=listed-a\n[status] model_origin=default\n[status] model_source=local\n"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("\n[status] auth=configured provider\n"),
        "{stdout}"
    );
}

#[test]
fn a_codex_login_reports_its_subscription_and_whether_it_expired() {
    let home = Home::new(Some(&codex_settings()));
    home.sign_in_to_codex(i64::MAX);
    let workspace = home.workspace().display().to_string();
    let (stdout, stderr) = home.succeeds(&["status"], &[]);
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        format!(
            "[status] model=gpt-5.4\n[status] model_origin=settings\n[status] model_source=Codex subscription\n[status] auth=Codex subscription\n[status] connected_providers=Codex\n[status] auth_refreshable=true\n{}{MCP_EMPTY_TEXT}",
            tail(&workspace)
        )
    );
    let (stdout, _) = home.succeeds(&["status", "--json"], &[]);
    assert_eq!(
        stdout,
        format!(
            r#"{{"kind":"status","model":"gpt-5.4","model_origin":"settings","model_source":"Codex subscription","auth":"Codex subscription","connected_providers":["codex"],"auth_refreshable":true,{},{MCP_EMPTY_JSON}}}
"#,
            json_tail(&workspace)
        )
    );

    home.sign_in_to_codex(1);
    let (stdout, _) = home.succeeds(&["status"], &[]);
    assert!(
        stdout.contains("\n[status] auth_refreshable=true\n[status] auth_expired=true\n[status] permission_mode=auto\n"),
        "{stdout}"
    );
    let (stdout, _) = home.succeeds(&["status", "--json"], &[]);
    assert!(
        stdout
            .contains(r#","auth_refreshable":true,"auth_expired":true,"permission_mode":"auto","#),
        "{stdout}"
    );
}

#[test]
fn a_missing_codex_login_shows_the_sign_in_help() {
    let home = Home::new(Some(&codex_settings()));
    let workspace = home.workspace().display().to_string();
    let (stdout, _) = home.succeeds(&["status"], &[]);
    assert_eq!(
        stdout,
        format!(
            "[status] model=gpt-5.4\n[status] model_origin=settings\n[status] model_source=Codex subscription\n[status] auth=missing\n[status] connected_providers=none\n[status] auth_refreshable=false\n[status] auth_help={MISSING_CODEX}\n{}{MCP_EMPTY_TEXT}",
            tail(&workspace)
        )
    );
    let (stdout, _) = home.succeeds(&["status", "--json"], &[]);
    assert_eq!(
        stdout,
        format!(
            r#"{{"kind":"status","model":"gpt-5.4","model_origin":"settings","model_source":"Codex subscription","auth":"missing","connected_providers":[],"auth_refreshable":false,"auth_help":"{MISSING_CODEX}",{},{MCP_EMPTY_JSON}}}
"#,
            json_tail(&workspace)
        )
    );
}

#[test]
fn the_environment_overrides_the_model_and_names_its_variable() {
    let home = Home::new(Some(&codex_settings()));
    let (stdout, _) = home.succeeds(&["status"], &[("OH_FX_MODEL", " gpt-run ")]);
    assert!(
        stdout.starts_with("[status] model=gpt-run\n[status] model_origin=OH_FX_MODEL\n"),
        "{stdout}"
    );
    let (stdout, _) = home.succeeds(
        &["status", "--json"],
        &[("OH_FX_PROVIDER", "codex"), ("OH_FX_MODEL", "gpt-run")],
    );
    assert!(
        stdout.starts_with(r#"{"kind":"status","model":"gpt-run","model_origin":"OH_FX_MODEL","#),
        "{stdout}"
    );
}

#[test]
fn settings_that_select_no_runnable_model_fail_like_startup() {
    let home = Home::new(Some(&json!({"provider": "codex"})));
    let output = home.status(&["status"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "oh-fx: no Codex model is selected; run `oh-fx provider codex` to choose one, or set a model for this run with --model or OH_FX_MODEL\n"
    );

    let home = Home::new(None);
    let output = home.status(&["status", "--json"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert!(
        text(&output.stderr)
            .starts_with("oh-fx: the gateway provider is not available in oh-fx yet;"),
        "{}",
        text(&output.stderr)
    );
}

#[test]
fn host_managed_authentication_is_active_for_every_provider() {
    let home = Home::new(Some(&codex_settings()));
    let (stdout, _) = home.succeeds(&["status"], &[("OH_FX_AUTH_MODE", "host-managed")]);
    assert!(
        stdout.contains("\n[status] auth=host managed\n[status] connected_providers=Codex\n[status] auth_refreshable=false\n[status] permission_mode=auto\n"),
        "{stdout}"
    );
}

#[test]
fn configuration_diagnostics_go_to_stderr_before_the_snapshot() {
    let home = Home::new(Some(&codex_settings()));
    home.sign_in_to_codex(i64::MAX);
    fs::write(home.workspace().join(".oh-fx.json"), r#"{"model":"x"}"#)
        .expect("write the project settings");
    let (stdout, stderr) = home.succeeds(&["status"], &[("OH_FX_PERMISSION_MODE", "ask")]);
    assert_eq!(
        stderr,
        "oh-fx: config project: ignored_project_user_only_setting; key=model\n"
    );
    assert!(
        stdout.contains("\n[status] permission_mode=ask\n"),
        "{stdout}"
    );
}

#[test]
fn profile_mcp_problems_are_reported_without_failing_the_command() {
    let home = Home::new(Some(&codex_settings()));
    home.sign_in_to_codex(i64::MAX);
    home.write_config(
        "mcp.json",
        r#"{"mcp":{"docs":{"command":"node"}},"mcpServers":{"other":{"command":"x"}}}"#,
    );
    let (stdout, _) = home.succeeds(&["status"], &[]);
    assert!(
        stdout.contains("[status] model_source=Codex subscription\n[status] mcp_config_warning=ignored_mcp_servers_alias key=mcpServers additional_matches=0\n[status] auth=Codex subscription\n"),
        "{stdout}"
    );
    assert!(
        stdout.ends_with("[status] mcp_servers=1 mcp_configuration_issues=0\n[status] mcp_server=docs source=profile scope=profile admission=not_applicable transport=stdio connection=not_checked authentication=not_checked\n"),
        "{stdout}"
    );
    let (stdout, _) = home.succeeds(&["status", "--json"], &[]);
    assert!(
        stdout.contains(r#""model_source":"Codex subscription","mcp_config_warning":{"cause":"ignored_mcp_servers_alias","key":"mcpServers","additional_matches":0},"auth":"Codex subscription","#),
        "{stdout}"
    );

    home.write_config("mcp.json", "{");
    let (stdout, _) = home.succeeds(&["status"], &[]);
    assert!(
        stdout.contains("[status] model_source=Codex subscription\n[status] mcp_config_error=McpConfigInvalidJson\n[status] auth=Codex subscription\n"),
        "{stdout}"
    );
    assert!(
        stdout.ends_with("[status] mcp_servers=0 mcp_configuration_issues=0\n[status] mcp_inspection_error=McpConfigInvalidJson\n"),
        "{stdout}"
    );
    let (stdout, _) = home.succeeds(&["status", "--json"], &[]);
    assert!(
        stdout.contains(r#""model_source":"Codex subscription","mcp_config_error":"McpConfigInvalidJson","auth":"#),
        "{stdout}"
    );
    assert!(
        stdout.ends_with(
            r#""configuration_issues":[],"inspection_error":"McpConfigInvalidJson"}}
"#
        ),
        "{stdout}"
    );
}

#[test]
fn skipped_workspace_servers_become_configuration_issues() {
    let home = Home::new(None);
    let workspace = home.workspace();
    let mut settings = codex_settings();
    settings["workspaces"] = json!({
        workspace.display().to_string(): {"enabledMcpjsonServers": ["tool"]},
    });
    home.write_config("settings.json", &settings.to_string());
    fs::write(
        workspace.join(".mcp.json"),
        r#"{"mcpServers":{"tool":{"command":"${MISSING_TOOL}"},"bad":7}}"#,
    )
    .expect("write the workspace MCP config");
    let (stdout, _) = home.succeeds(&["status"], &[]);
    assert!(
        stdout.ends_with("[status] mcp_servers=0 mcp_configuration_issues=2\n[status] mcp_configuration_issue=.mcp.json server 'bad' was skipped: invalid_entry.\n[status] mcp_configuration_issue=.mcp.json server 'tool' field command requires environment variable 'MISSING_TOOL'; set it or use ${MISSING_TOOL:-default}.\n"),
        "{stdout}"
    );
}

#[test]
fn status_ignores_the_v2_session_store_selection() {
    let home = Home::new(Some(&codex_settings()));
    let output = home.status(&["--sessions-v2", "status"], &[]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let output = home.status(&["status"], &[("OH_FX_SESSIONS_V2", "1")]);
    assert!(output.status.success(), "{}", text(&output.stderr));
}

#[test]
fn a_saved_grok_login_is_listed_after_codex() {
    let home = Home::new(Some(&codex_settings()));
    home.sign_in_to_codex(i64::MAX);
    let grok = home.root.join("data/oh-fx/grok-auth.json");
    fs::write(&grok, "{}").expect("write the Grok login");
    fs::set_permissions(&grok, fs::Permissions::from_mode(0o600))
        .expect("make the Grok login private");
    let (stdout, _) = home.succeeds(&["status"], &[]);
    assert!(
        stdout.contains("\n[status] connected_providers=Codex, Grok\n"),
        "{stdout}"
    );
    let (stdout, _) = home.succeeds(&["status", "--json"], &[]);
    assert!(
        stdout.contains(r#","connected_providers":["codex","grok"],"#),
        "{stdout}"
    );
}

#[test]
fn a_hostile_workspace_name_is_encoded_in_text_and_kept_raw_in_json() {
    let home = Home::new(Some(&codex_settings()));
    let hostile = home.root.join("work\u{1b}[2J\n[status] auth=forged");
    fs::create_dir_all(&hostile).expect("create the hostile workspace");
    let output = home.status_in(&hostile, &["status"], &[]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(!stdout.contains('\u{1b}'), "{stdout:?}");
    assert!(
        stdout.contains(&format!(
            "\n[status] workspace={}/work\\x1b[2J\\x0a[status] auth=forged\n",
            home.root.display()
        )),
        "{stdout}"
    );
    assert_eq!(
        stdout
            .lines()
            .filter(|line| line.starts_with("[status] auth="))
            .count(),
        1,
        "{stdout}"
    );
    let output = home.status_in(&hostile, &["status", "--json"], &[]);
    let json: Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(json["workspace"], hostile.display().to_string().as_str());
}
