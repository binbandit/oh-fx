use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use ofx_testkit::{FakeServer, RecordedRequest, Reply};
use serde_json::{Value, json};

const API_KEY: &str = "vck-gateway-0123456789abcdef";
const OIDC_TOKEN: &str = "oidc-token-0123456789abcdef";
const CHAT_PATH: &str = "/v4/ai/language-model";
const DEFAULT_MODEL: &str = "spacexai/grok-4.7";
const MISSING: &str = "oh-fx needs access to Vercel AI Gateway. Run oh-fx setup to use an API key, or set AI_GATEWAY_API_KEY.";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
}

impl Home {
    fn with_settings(settings: &Value) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory.path().to_owned();
        let config = root.join("config/oh-fx");
        let workspace = root.join("workspace");
        fs::create_dir_all(&config).expect("create the config directory");
        fs::create_dir_all(&workspace).expect("create the workspace");
        fs::write(config.join("settings.json"), settings.to_string()).expect("write settings.json");
        Self {
            _directory: directory,
            root,
            workspace,
        }
    }

    fn run(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
            .current_dir(&self.workspace)
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

fn reply(text: &str) -> Reply {
    Reply::sse(&[
        json!({"type": "text-delta", "id": "t", "delta": text}).to_string(),
        r#"{"type":"finish","finishReason":{"unified":"stop"},"usage":{"inputTokens":{"total":12},"outputTokens":{"total":3}}}"#.to_owned(),
    ])
}

fn chat_url(server: &FakeServer) -> String {
    format!("http://{}{CHAT_PATH}", server.address())
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn only_request(server: &FakeServer) -> RecordedRequest {
    let mut requests = server.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    requests.remove(0)
}

#[test]
fn ask_streams_a_gateway_reply_with_the_environment_key() {
    let server = FakeServer::start([reply("from the gateway")]);
    let url = chat_url(&server);
    let home = Home::with_settings(&json!({}));
    let output = home.run(
        &["ask", "--no-save", "hello"],
        &[
            ("AI_GATEWAY_API_KEY", API_KEY),
            ("OH_FX_E2E_GATEWAY_CHAT_URL", &url),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "from the gateway");
    let request = only_request(&server);
    assert_eq!(request.path, CHAT_PATH);
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {API_KEY}").as_str())
    );
    assert_eq!(request.header("ai-language-model-id"), Some(DEFAULT_MODEL));
    assert_eq!(request.header("ai-language-model-streaming"), Some("true"));
    let body = request.json();
    assert_eq!(body["toolChoice"], json!({"type": "auto"}));
    assert_eq!(body["providerOptions"]["gateway"]["caching"], "auto");
    let prompt = body["prompt"].as_array().expect("a prompt");
    assert_eq!(
        prompt.last(),
        Some(&json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}))
    );
    assert!(!stderr(&output).contains(API_KEY));
}

#[test]
fn the_oidc_token_outranks_the_api_key_and_blank_values_do_not_count() {
    for (oidc, bearer) in [(OIDC_TOKEN, OIDC_TOKEN), (" \t", API_KEY)] {
        let server = FakeServer::start([reply("ok")]);
        let url = chat_url(&server);
        let home = Home::with_settings(&json!({}));
        let output = home.run(
            &["ask", "--no-save", "hello"],
            &[
                ("VERCEL_OIDC_TOKEN", oidc),
                ("AI_GATEWAY_API_KEY", API_KEY),
                ("OH_FX_E2E_GATEWAY_CHAT_URL", &url),
            ],
        );
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(
            only_request(&server).header("authorization"),
            Some(format!("Bearer {bearer}").as_str())
        );
    }
}

#[test]
fn host_managed_authentication_sends_no_authorization() {
    let server = FakeServer::start([reply("hosted")]);
    let url = chat_url(&server);
    let home = Home::with_settings(&json!({}));
    let output = home.run(
        &["ask", "--no-save", "hello"],
        &[
            ("OH_FX_AUTH_MODE", "host-managed"),
            ("OH_FX_E2E_GATEWAY_CHAT_URL", &url),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "hosted");
    assert_eq!(only_request(&server).header("authorization"), None);
}

#[test]
fn a_missing_gateway_credential_explains_itself_before_any_request() {
    let server = FakeServer::start([]);
    let url = chat_url(&server);
    let home = Home::with_settings(&json!({}));
    for blank in [None, Some(" \r\n")] {
        let mut environment = vec![("OH_FX_E2E_GATEWAY_CHAT_URL", url.as_str())];
        environment.extend(blank.map(|value| ("AI_GATEWAY_API_KEY", value)));
        let output = home.run(&["ask", "hello"], &environment);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(stderr(&output), format!("oh-fx ask: {MISSING}\n"));
        assert_eq!(stdout(&output), "");
    }
    let output = home.run(
        &["ask", "--json", "hello"],
        &[("OH_FX_E2E_GATEWAY_CHAT_URL", &url)],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), format!("oh-fx ask: {MISSING}\n"));
    assert_eq!(
        stdout(&output),
        "{\"output\":\"\",\"final_output\":\"\",\"exit_code\":1,\"model\":\"\",\"resolved_provider\":null,\"session_id\":\"\",\"steps\":0,\"tool_calls\":[],\"usage\":{\"input_tokens\":null,\"output_tokens\":null},\"error\":\"MissingCredentials\"}\n"
    );
    assert!(server.requests().is_empty());
}

#[test]
fn a_rejected_key_names_the_environment_variable_it_came_from() {
    let body =
        format!(r#"{{"error":{{"message":"bad key {API_KEY}","type":"authentication_error"}}}}"#);
    let server = FakeServer::start([Reply::status(401, body.clone()), Reply::status(401, body)]);
    let url = chat_url(&server);
    let home = Home::with_settings(&json!({}));
    let environment = [
        ("AI_GATEWAY_API_KEY", API_KEY),
        ("OH_FX_E2E_GATEWAY_CHAT_URL", url.as_str()),
    ];
    let output = home.run(&["ask", "--no-save", "hello"], &environment);
    assert_eq!(output.status.code(), Some(1));
    let text = stderr(&output);
    assert!(
        text.starts_with("oh-fx ask: AI_GATEWAY_API_KEY authentication failed · HTTP 401\n"),
        "{text}"
    );
    assert!(!text.contains(API_KEY), "{text}");
    let output = home.run(&["ask", "--no-save", "--json", "hello"], &environment);
    let result: Value = serde_json::from_slice(&output.stdout).expect("a JSON result");
    assert_eq!(
        result["auth_failure"],
        json!({"source": "AI_GATEWAY_API_KEY", "reason": "http_unauthorized", "http_status": 401})
    );
    assert_eq!(result["model"], DEFAULT_MODEL);
}

#[test]
fn the_gateway_model_follows_the_flag_then_the_environment_then_settings() {
    let server = FakeServer::start([reply("a"), reply("b"), reply("c")]);
    let url = chat_url(&server);
    let home = Home::with_settings(&json!({"models": {"gateway": "openai/gpt-5.6-sol"}}));
    let base = [
        ("AI_GATEWAY_API_KEY", API_KEY),
        ("OH_FX_E2E_GATEWAY_CHAT_URL", url.as_str()),
    ];
    let with_model = [base[0], base[1], ("OH_FX_MODEL", "zai/glm-5.2")];
    for (args, environment) in [
        (
            &["ask", "--no-save", "--model", "xai/grok-5", "hi"][..],
            &with_model[..],
        ),
        (&["ask", "--no-save", "hi"][..], &with_model[..]),
        (&["ask", "--no-save", "hi"][..], &base[..]),
    ] {
        let output = home.run(args, environment);
        assert!(output.status.success(), "{}", stderr(&output));
    }
    let models: Vec<String> = server
        .requests()
        .iter()
        .map(|request| {
            request
                .header("ai-language-model-id")
                .expect("a model header")
                .to_owned()
        })
        .collect();
    assert_eq!(models, ["xai/grok-5", "zai/glm-5.2", "openai/gpt-5.6-sol"]);
}

#[test]
fn the_end_to_end_url_outranks_the_gateway_url_override() {
    let preferred = FakeServer::start([reply("preferred")]);
    let other = FakeServer::start([reply("other")]);
    let home = Home::with_settings(&json!({}));
    let output = home.run(
        &["ask", "--no-save", "hello"],
        &[
            ("AI_GATEWAY_API_KEY", API_KEY),
            ("OH_FX_E2E_GATEWAY_CHAT_URL", &chat_url(&preferred)),
            ("OH_FX_GATEWAY_CHAT_URL", &chat_url(&other)),
        ],
    );
    assert_eq!(stdout(&output), "preferred");
    assert!(other.requests().is_empty());
    let output = home.run(
        &["ask", "--no-save", "hello"],
        &[
            ("AI_GATEWAY_API_KEY", API_KEY),
            ("OH_FX_GATEWAY_CHAT_URL", &chat_url(&other)),
        ],
    );
    assert_eq!(stdout(&output), "other");
}
