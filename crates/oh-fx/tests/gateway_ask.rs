use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use ofx_testkit::{FakeServer, RecordedRequest, Reply};
use serde_json::{Value, json};

const API_KEY: &str = "vck-gateway-0123456789abcdef";
const OIDC_TOKEN: &str = "oidc-token-0123456789abcdef";
const CHAT_PATH: &str = "/v4/ai/language-model";
const DEFAULT_MODEL: &str = "spacexai/grok-4.7";
const MISSING: &str = "oh-fx needs access to Vercel AI Gateway. Run oh-fx login to sign in, oh-fx setup to use an API key, or set AI_GATEWAY_API_KEY.";

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

fn routed_ask(home: &Home, environment: &[(&str, &str)]) -> Value {
    routed_ask_with(home, &[], environment)
}

fn routed_ask_with(home: &Home, flags: &[&str], environment: &[(&str, &str)]) -> Value {
    let server = FakeServer::start([reply("routed")]);
    let url = chat_url(&server);
    let mut variables = vec![
        ("AI_GATEWAY_API_KEY", API_KEY),
        ("OH_FX_E2E_GATEWAY_CHAT_URL", url.as_str()),
    ];
    variables.extend_from_slice(environment);
    let mut args = vec!["ask", "--no-save"];
    args.extend_from_slice(flags);
    args.push("hello");
    let output = home.run(&args, &variables);
    assert!(output.status.success(), "{}", stderr(&output));
    only_request(&server).json()["providerOptions"]["gateway"].clone()
}

#[test]
fn provider_routing_from_settings_the_project_and_the_environment_reaches_the_gateway() {
    let home = Home::with_settings(&json!({
        "provider_order": ["bedrock", "azure"],
        "provider_strict": true,
    }));
    assert_eq!(
        routed_ask(&home, &[]),
        json!({"caching": "auto", "only": ["bedrock", "azure"]})
    );
    assert_eq!(
        routed_ask(
            &home,
            &[
                ("OH_FX_PROVIDER_ORDER", " vertex, azure "),
                ("OH_FX_PROVIDER_STRICT", "off"),
            ],
        ),
        json!({"caching": "auto", "order": ["vertex", "azure"]})
    );
    let trace = home.root.join("trace.log");
    assert_eq!(
        routed_ask(
            &home,
            &[
                ("OH_FX_PROVIDER_ORDER", "Bad Slug"),
                ("OH_FX_PROVIDER_STRICT", "sometimes"),
                ("OH_FX_TRACE_LOG", trace.to_str().expect("a text path")),
            ],
        ),
        json!({"caching": "auto", "only": ["bedrock", "azure"]})
    );
    let log = fs::read_to_string(&trace).expect("read the trace log");
    for variable in ["OH_FX_PROVIDER_ORDER", "OH_FX_PROVIDER_STRICT"] {
        assert!(
            log.lines().any(|line| line.contains("[config]")
                && line.ends_with(&format!("ignoring invalid {variable} value"))),
            "{log}"
        );
    }
    let project = Home::with_settings(&json!({}));
    fs::write(
        project.workspace.join(".oh-fx.json"),
        json!({"provider_order": ["azure"]}).to_string(),
    )
    .expect("write the project file");
    assert_eq!(
        routed_ask(&project, &[]),
        json!({"caching": "auto", "order": ["azure"]})
    );
    assert_eq!(
        routed_ask(&project, &[("OH_FX_PROVIDER_STRICT", "1")]),
        json!({"caching": "auto", "only": ["azure"]})
    );
}

const GENERATION_ID: &str = "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn billed_reply(cost: Option<&str>) -> Reply {
    billed_reply_as(cost, "zai/glm-5.2")
}

fn billed_reply_as(cost: Option<&str>, later_model: &str) -> Reply {
    let mut gateway = json!({
        "generationId": GENERATION_ID,
        "routing": {"canonicalSlug": "zai/glm-5.2"},
    });
    if let Some(cost) = cost {
        gateway["cost"] = json!(cost);
    }
    Reply::sse(&[
        json!({"type": "response-metadata", "timestamp": "2026-04-01T12:11:07Z", "modelId": "zai/glm-5.2"}).to_string(),
        json!({"type": "response-metadata", "modelId": later_model}).to_string(),
        json!({"type": "text-delta", "id": "t", "delta": "billed"}).to_string(),
        json!({
            "type": "finish",
            "finishReason": {"unified": "stop"},
            "usage": {"inputTokens": {"total": 12, "cacheRead": 4}, "outputTokens": {"total": 3}},
            "providerMetadata": {"gateway": gateway},
        })
        .to_string(),
    ])
}

fn saved_usage(home: &Home, output: &Output) -> Value {
    let result: Value = serde_json::from_str(&stdout(output)).expect("a JSON result");
    let id = result["session_id"].as_str().expect("a session id");
    let usage = home
        .root
        .join("data/oh-fx/sessions")
        .join(id)
        .join("usage-v2.json");
    serde_json::from_str::<Value>(&fs::read_to_string(usage).expect("read usage-v2.json"))
        .expect("session usage")["snapshot"]
        .clone()
}

fn settled(log: &str, line: &str) -> bool {
    log.lines()
        .any(|logged| logged.contains("[session]") && logged.ends_with(line))
}

#[test]
fn a_gateway_finish_settles_exact_deferred_or_unidentified_usage() {
    let server = FakeServer::start([
        billed_reply(Some("0.0125")),
        billed_reply(None),
        billed_reply_as(Some("0.0125"), "zai/glm-5.3"),
        reply("plain"),
        billed_reply(None),
    ]);
    let url = chat_url(&server);
    let home = Home::with_settings(&json!({"session_titles": false}));
    let trace = home.root.join("trace.log");
    let environment = [
        ("AI_GATEWAY_API_KEY", API_KEY),
        ("OH_FX_E2E_GATEWAY_CHAT_URL", url.as_str()),
        ("OH_FX_TRACE_LOG", trace.to_str().expect("a text path")),
    ];
    let exact = home.run(&["ask", "--json", "first"], &environment);
    assert!(exact.status.success(), "{}", stderr(&exact));
    let snapshot = saved_usage(&home, &exact);
    assert_eq!(snapshot["billing"], "complete", "{snapshot}");
    assert_eq!(snapshot["models"][0]["model"], "zai/glm-5.2", "{snapshot}");
    assert_eq!(snapshot["input_tokens"], 12, "{snapshot}");
    assert_eq!(snapshot["cache_read_tokens"], 4, "{snapshot}");
    assert_eq!(snapshot["total_cost"], 0.0125, "{snapshot}");
    let log = fs::read_to_string(&trace).expect("read the trace log");
    assert!(!log.contains("stream billing ignored"), "{log}");
    let exact_line = format!("usage exact generation settled sequence=1 id={GENERATION_ID}");
    assert!(settled(&log, &exact_line), "{log}");

    let unpriced = home.run(&["ask", "--json", "second"], &environment);
    assert!(unpriced.status.success(), "{}", stderr(&unpriced));
    let snapshot = saved_usage(&home, &unpriced);
    assert_eq!(snapshot["billing"], "pending", "{snapshot}");
    let pending = &snapshot["pending"][0];
    assert_eq!(pending["id"], GENERATION_ID, "{snapshot}");
    assert_eq!(pending["provider"], "gateway", "{snapshot}");
    assert_eq!(
        pending["origin"], "https://ai-gateway.vercel.sh",
        "{snapshot}"
    );
    assert_eq!(pending["team"], Value::Null, "{snapshot}");
    assert_eq!(
        pending["credential_source"], "ai_gateway_api_key",
        "{snapshot}"
    );
    assert_eq!(
        pending["credential_identity"].as_str().map(str::len),
        Some(64),
        "{snapshot}"
    );
    let log = fs::read_to_string(&trace).expect("read the trace log");
    assert!(
        log.lines().any(|line| line.contains("[sse]")
            && line.ends_with("stream billing ignored reason=invalid_terminal_metadata")),
        "{log}"
    );
    let queued_line = format!("usage generation queued sequence=1 id={GENERATION_ID}");
    assert!(settled(&log, &queued_line), "{log}");

    for (prompt, line) in [
        (
            "third",
            "usage generation ignored sequence=1 metadata_invalid=true",
        ),
        (
            "fourth",
            "usage billing incomplete sequence=1 reason=generation_identity_missing",
        ),
    ] {
        let unidentified = home.run(&["ask", "--json", prompt], &environment);
        assert!(unidentified.status.success(), "{}", stderr(&unidentified));
        let snapshot = saved_usage(&home, &unidentified);
        assert_eq!(snapshot["billing"], "incomplete", "{snapshot}");
        assert_eq!(snapshot["pending"], json!([]), "{snapshot}");
        let log = fs::read_to_string(&trace).expect("read the trace log");
        assert!(settled(&log, line), "{log}");
    }

    let based = [
        environment[0],
        environment[1],
        ("OH_FX_GATEWAY_BASE_URL", "http://127.0.0.1:9/"),
    ];
    let loopback = home.run(&["ask", "--json", "fifth"], &based);
    assert!(loopback.status.success(), "{}", stderr(&loopback));
    let snapshot = saved_usage(&home, &loopback);
    assert_eq!(
        snapshot["pending"][0]["origin"], "http://127.0.0.1:9",
        "{snapshot}"
    );
}

fn ultra_catalog() -> FakeServer {
    FakeServer::start([Reply::status(
        200,
        json!({"data": [{
            "id": "openai/gpt-6-astra",
            "type": "language",
            "owned_by": "openai",
            "tags": ["tool-use"],
            "pricing": {"service_tiers": {"ultrafast": {"input": "0.00006", "output": "0.0003"}}},
        }]})
        .to_string(),
    )])
}

fn finished(unified: &str) -> String {
    json!({
        "type": "finish",
        "finishReason": {"unified": unified},
        "usage": {"inputTokens": {"total": 12}, "outputTokens": {"total": 3}},
        "providerMetadata": {"gateway": {"serviceTier": "ultrafast"}},
    })
    .to_string()
}

#[test]
fn ask_ultrafast_requests_the_ultra_tier_for_the_turn_and_its_subagent() {
    let catalog = ultra_catalog();
    let chat = FakeServer::start([
        Reply::sse(&[
            json!({
                "type": "tool-call",
                "toolCallId": "call_1",
                "toolName": "subagent",
                "input": json!({"request": {"action": "run", "task": "inspect auth"}}).to_string(),
            })
            .to_string(),
            finished("tool-calls"),
        ]),
        Reply::sse(&[
            json!({"type": "text-delta", "id": "t", "delta": "child report"}).to_string(),
            finished("stop"),
        ]),
        Reply::sse(&[
            json!({"type": "text-delta", "id": "t", "delta": "parent done"}).to_string(),
            finished("stop"),
        ]),
    ]);
    let home = Home::with_settings(&json!({
        "models": {"gateway": "openai/gpt-6-astra"},
        "session_titles": false,
    }));
    let chat_url = chat_url(&chat);
    let models_url = format!("http://{}/coding-agent/v1/models", catalog.address());
    let output = home.run(
        &["ask", "--ultrafast", "check the auth module"],
        &[
            ("AI_GATEWAY_API_KEY", API_KEY),
            ("OH_FX_E2E_GATEWAY_CHAT_URL", chat_url.as_str()),
            ("OH_FX_E2E_GATEWAY_MODELS_URL", models_url.as_str()),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "parent done");
    let requests = chat.requests();
    assert_eq!(requests.len(), 3);
    for request in requests {
        assert_eq!(
            request.json()["providerOptions"]["openai"],
            json!({"serviceTier": "ultrafast"})
        );
    }
}

#[test]
fn ask_ultrafast_on_a_model_without_the_tier_fails_before_any_request() {
    let catalog = ultra_catalog();
    let chat = FakeServer::start([]);
    let home = Home::with_settings(&json!({"models": {"gateway": "zai/glm-5.2"}}));
    let chat_url = chat_url(&chat);
    let models_url = format!("http://{}/coding-agent/v1/models", catalog.address());
    let output = home.run(
        &["ask", "--no-save", "--ultrafast", "hello"],
        &[
            ("AI_GATEWAY_API_KEY", API_KEY),
            ("OH_FX_E2E_GATEWAY_CHAT_URL", chat_url.as_str()),
            ("OH_FX_E2E_GATEWAY_MODELS_URL", models_url.as_str()),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("oh-fx: UltrafastUnavailable"),
        "{}",
        stderr(&output)
    );
    assert!(chat.requests().is_empty());
}

#[test]
fn ask_routing_flags_outrank_the_environment_and_settings() {
    let home = Home::with_settings(&json!({
        "provider_order": ["bedrock"],
        "provider_strict": true,
    }));
    let environment = [("OH_FX_PROVIDER_ORDER", "azure")];
    assert_eq!(
        routed_ask_with(
            &home,
            &["--provider-order", "vertex,openai", "--no-provider-strict"],
            &environment,
        ),
        json!({"caching": "auto", "order": ["vertex", "openai"]})
    );
    assert_eq!(
        routed_ask_with(&home, &["--provider-strict"], &environment),
        json!({"caching": "auto", "only": ["azure"]})
    );
}

#[test]
fn the_ultra_setting_and_its_variable_choose_the_tier_for_ask() {
    let home = Home::with_settings(&json!({
        "models": {"gateway": "openai/gpt-6-astra"},
        "ultrafast_mode": true,
    }));
    let run = |environment: &[(&str, &str)]| {
        let catalog = ultra_catalog();
        let chat = FakeServer::start([reply("tiered")]);
        let chat_url = chat_url(&chat);
        let models_url = format!("http://{}/coding-agent/v1/models", catalog.address());
        let mut variables = vec![
            ("AI_GATEWAY_API_KEY", API_KEY),
            ("OH_FX_E2E_GATEWAY_CHAT_URL", chat_url.as_str()),
            ("OH_FX_E2E_GATEWAY_MODELS_URL", models_url.as_str()),
        ];
        variables.extend_from_slice(environment);
        let output = home.run(&["ask", "--no-save", "hello"], &variables);
        let tiers: Vec<Value> = chat
            .requests()
            .iter()
            .map(|request| request.json()["providerOptions"]["openai"].clone())
            .collect();
        (output, tiers)
    };
    let (setting, tiers) = run(&[]);
    assert!(setting.status.success(), "{}", stderr(&setting));
    assert_eq!(tiers, [json!({"serviceTier": "ultrafast"})]);
    let (off, tiers) = run(&[("OH_FX_ULTRAFAST", " off ")]);
    assert!(off.status.success(), "{}", stderr(&off));
    assert_eq!(tiers, [Value::Null]);
    let (invalid, tiers) = run(&[("OH_FX_ULTRAFAST", "sometimes")]);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(
        stderr(&invalid).contains("oh-fx: InvalidUltrafastOverride"),
        "{}",
        stderr(&invalid)
    );
    assert!(tiers.is_empty());
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

#[test]
fn ask_takes_the_reasoning_effort_the_gateway_catalog_offers() {
    let chat = FakeServer::start([reply("reasoned"), reply("plain")]);
    let catalog = FakeServer::start([
        Reply::status(
            200,
            r#"{"data":[{"id":"spacexai/grok-4.7","type":"language","tags":["tool-use"],"reasoning_options":[{"type":"effort","values":["low","high"]}]}]}"#,
        ),
        Reply::status(200, r#"{"data":[]}"#),
    ]);
    let chat_url = chat_url(&chat);
    let models_url = format!("http://{}/coding-agent/v1/models", catalog.address());
    let home = Home::with_settings(&json!({}));
    let trace = home.root.join("trace.log");
    let trace_log = trace.display().to_string();
    let environment = [
        ("AI_GATEWAY_API_KEY", API_KEY),
        ("OH_FX_E2E_GATEWAY_CHAT_URL", chat_url.as_str()),
        ("OH_FX_E2E_GATEWAY_MODELS_URL", models_url.as_str()),
        ("OH_FX_TRACE_LOG", trace_log.as_str()),
    ];
    for expected in [Some("high"), None] {
        let output = home.run(
            &["ask", "--no-save", "--effort", "high", "hello"],
            &environment,
        );
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(!stderr(&output).contains(API_KEY));
        let requests = chat.requests();
        let body = requests.last().expect("a chat request").json();
        assert_eq!(body["reasoning"].as_str(), expected, "{body}");
    }
    let lookups = catalog.requests();
    assert_eq!(lookups.len(), 2);
    let log = fs::read_to_string(&trace).expect("read the trace log");
    for outcome in ["ready_hit", "missing_entry"] {
        assert!(
            log.contains(&format!(
                "interactive model catalog lookup outcome={outcome} model={DEFAULT_MODEL}"
            )),
            "{log}"
        );
    }
    assert!(!log.contains(API_KEY));
    assert_eq!(
        lookups[0].header("authorization"),
        Some(format!("Bearer {API_KEY}").as_str())
    );
}
