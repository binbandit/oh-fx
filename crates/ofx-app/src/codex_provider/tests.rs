use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ofx_agent::{Agent, AgentConfig, TurnFailure, TurnReport};
use ofx_contract::{
    AutoCompactPercent, ModelRecoveryCause, PermissionMode, ProviderErrorKind, Tool,
    ToolResultStatus, TurnOutcome, UiEvent,
};
use ofx_exec::{ManagedExecutions, SessionSupervisor};
use ofx_permissions::PermissionPolicy;
use ofx_testkit::{FakeServer, RecordedRequest, Reply};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::context::{GATEWAY_SYSTEM_PROMPT, HostRuntimeContext};
use crate::model_cache_runtime::ModelSource;
use crate::tool_set::{self, ToolHooks};

const SAVED_TOKEN: &str = "eyJhbGciOiJub25lIn0.c2F2ZWQtYWNjZXNz.c2lnbmF0dXJl";
const FRESH_TOKEN: &str = "eyJhbGciOiJub25lIn0.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF90ZXN0In0sImV4cCI6NDEwMjQ0NDgwMCwibWFya2VyIjoiZnJlc2gifQ.c2lnbmF0dXJl";
const REFRESH_TOKEN: &str = "rt-refresh-secret-0123456789";
const ROTATED_REFRESH_TOKEN: &str = "rt-rotated-secret-9876543210";
const ACCOUNT: &str = "acct_test";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const FAR_FUTURE_MS: i64 = 4_102_444_800_000;
const MODEL: &str = "gpt-5.4";

struct Fixture {
    _directory: tempfile::TempDir,
    paths: ProfilePaths,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory.path();
        let paths = ProfilePaths {
            config: root.join("config/oh-fx"),
            data: root.join("data/oh-fx"),
            state: root.join("state/oh-fx"),
            cache: root.join("cache/oh-fx"),
        };
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("create the workspace");
        fs::write(workspace.join("README.md"), "# Readme\n").expect("write the readme");
        Self {
            _directory: directory,
            paths,
            workspace,
        }
    }

    fn credential_file(&self) -> PathBuf {
        self.paths.data.join("chatgpt-auth.json")
    }

    fn write_session(&self, expires_at_ms: i64, mode: u32) {
        fs::create_dir_all(&self.paths.data).expect("create the data directory");
        fs::set_permissions(&self.paths.data, fs::Permissions::from_mode(0o700))
            .expect("make the data directory private");
        let session = json!({
            "version": 1,
            "access_token": SAVED_TOKEN,
            "refresh_token": REFRESH_TOKEN,
            "expires_at_ms": expires_at_ms,
            "account_id": ACCOUNT,
        });
        fs::write(self.credential_file(), format!("{session}\n")).expect("write the session");
        fs::set_permissions(self.credential_file(), fs::Permissions::from_mode(mode))
            .expect("set the session mode");
    }

    fn saved(&self) -> Value {
        serde_json::from_slice(&fs::read(self.credential_file()).expect("read the session"))
            .expect("parse the session")
    }

    async fn provider(
        &self,
        auth: &FakeServer,
        codex: &FakeServer,
    ) -> Result<CodexProvider, CodexUnavailable> {
        self.subscription(subscription_endpoints(auth, codex))
            .await
            .map(|subscription| subscription.provider)
    }

    async fn subscription(
        &self,
        endpoints: SubscriptionEndpoints,
    ) -> Result<CodexSubscription, CodexUnavailable> {
        codex_subscription(
            Some(&self.paths),
            "oh-fx/test",
            endpoints,
            None,
            &CancellationToken::new(),
        )
        .await
    }

    fn canonical_workspace(&self) -> PathBuf {
        fs::canonicalize(&self.workspace).expect("canonicalize the workspace")
    }

    fn agent(&self, provider: CodexProvider) -> Agent {
        self.agent_with(provider, agent_config(MODEL, None, false))
    }

    fn agent_with(&self, provider: CodexProvider, config: AgentConfig) -> Agent {
        self.agent_on(Arc::new(provider), config)
    }

    fn agent_on(&self, provider: Arc<dyn ModelProvider>, config: AgentConfig) -> Agent {
        let workspace = self.canonical_workspace();
        let tools = ask_tools(&workspace);
        let permissions = PermissionPolicy::new(PermissionMode::Auto, workspace.clone());
        let context = HostRuntimeContext::new(workspace, PermissionMode::Auto, false);
        Agent::new(
            provider,
            tools,
            Arc::new(context),
            Arc::new(permissions),
            config,
        )
    }
}

fn subscription_endpoints(auth: &FakeServer, codex: &FakeServer) -> SubscriptionEndpoints {
    SubscriptionEndpoints {
        chatgpt: ChatGptEndpoints {
            issuer: auth.base_url(),
            token_url: format!("{}/oauth/token", auth.base_url()),
            callback_ports: vec![0],
        },
        codex: CodexEndpoints {
            responses: format!("{}/backend-api/codex/responses", codex.base_url()),
        },
        ..SubscriptionEndpoints::default()
    }
}

fn agent_config(model: &str, effort: Option<&str>, fast_mode: bool) -> AgentConfig {
    AgentConfig {
        model: model.to_owned(),
        system_prompt: GATEWAY_SYSTEM_PROMPT.to_owned(),
        max_output_tokens: None,
        step_limit: 0,
        reasoning_effort: effort.map(str::to_owned),
        fast_mode,
        auto_compact_percent: AutoCompactPercent::resolve(None, None),
    }
}

fn ask_tools(workspace: &Path) -> Vec<Arc<dyn Tool>> {
    let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent"));
    tool_set::ask_tools(
        workspace,
        &executions,
        None,
        &PermissionMode::Auto.into(),
        crate::skills::rootless_skill_tool(),
        crate::skills::rootless_capability_search(),
        ToolHooks::default(),
    )
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after the epoch")
            .as_millis(),
    )
    .expect("the clock fits in i64")
}

fn token_reply(access: &str) -> Reply {
    Reply::status(
        200,
        json!({"access_token": access, "refresh_token": ROTATED_REFRESH_TOKEN, "expires_in": 3600})
            .to_string(),
    )
}

fn events(values: &[Value]) -> Vec<String> {
    values.iter().map(Value::to_string).collect()
}

fn tool_step_events() -> Vec<String> {
    events(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}),
        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"Thinking"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"opaque-cipher"}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1","phase":"commentary"}}),
        json!({"type":"response.output_text.delta","output_index":1,"delta":"I will read it."}),
        json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"path\":\"README.md\"}"}),
        json!({"type":"response.function_call_arguments.done","output_index":2,"arguments":"{\"path\":\"README.md\"}"}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":5}}}),
    ])
}

fn text_events(text: &str) -> Vec<String> {
    events(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_2","phase":"final_answer"}}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":text}),
        json!({"type":"response.completed","response":{"id":"resp_2","status":"completed","usage":{"input_tokens":20,"output_tokens":3}}}),
    ])
}

async fn run(agent: &mut Agent, prompt: &str) -> (TurnReport, Vec<UiEvent>) {
    let mut seen = Vec::new();
    let report = agent
        .run_turn(
            prompt,
            &mut |event| seen.push(event),
            &CancellationToken::new(),
        )
        .await;
    (report, seen)
}

fn assistant_text(seen: &[UiEvent]) -> String {
    seen.iter()
        .filter_map(|event| match event {
            UiEvent::AssistantText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn assert_codex_request(request: &RecordedRequest, token: &str) {
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/backend-api/codex/responses");
    let bearer = format!("Bearer {token}");
    assert_eq!(request.header("authorization"), Some(bearer.as_str()));
    assert_eq!(request.header("chatgpt-account-id"), Some(ACCOUNT));
    assert_eq!(request.header("originator"), Some("oh-fx"));
    assert_eq!(
        request.header("openai-beta"),
        Some("responses=experimental")
    );
    assert_eq!(request.header("accept"), Some("text/event-stream"));
    assert_eq!(request.header("user-agent"), Some("oh-fx/test"));
}

fn assert_offers_the_ask_tools(body: &Value, workspace: &Path) {
    let offered: Vec<&str> = body["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    let ask_tools: Vec<String> = ask_tools(workspace)
        .iter()
        .filter(|tool| !tool.provider_executed())
        .map(|tool| tool.spec().name.clone())
        .collect();
    assert_eq!(offered, ask_tools);
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

#[tokio::test]
async fn ask_refreshes_an_expired_login_and_streams_a_tool_step_through_responses() {
    let fixture = Fixture::new();
    fixture.write_session(now_ms() - 1_000, 0o600);
    let auth = FakeServer::start([token_reply(FRESH_TOKEN)]);
    let codex = FakeServer::start([
        Reply::sse(&tool_step_events()),
        Reply::sse(&text_events("Done.")),
    ]);
    let provider = fixture
        .provider(&auth, &codex)
        .await
        .expect("the refreshed login builds a provider");
    let debug = format!("{provider:?}");
    let mut agent = fixture.agent(provider);
    let (report, seen) = run(&mut agent, "Read README.md").await;

    assert_eq!(report.outcome, TurnOutcome::Completed, "{report:?}");
    assert_eq!(report.final_text, "Done.");
    assert_eq!(assistant_text(&seen), "I will read it.Done.");
    assert!(seen.iter().any(|event| matches!(
        event,
        UiEvent::ToolFinished { tool_name, status: ToolResultStatus::Success, .. }
            if tool_name == "read_file"
    )));

    let refreshes = auth.requests();
    assert_eq!(refreshes.len(), 1);
    assert_eq!(refreshes[0].path, "/v1/oauth/token");
    assert_eq!(
        refreshes[0].body_text(),
        format!(
            r#"{{"client_id":"{CLIENT_ID}","grant_type":"refresh_token","refresh_token":"{REFRESH_TOKEN}"}}"#
        )
    );

    let requests = codex.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_codex_request(request, FRESH_TOKEN);
    }
    let first = requests[0].json();
    assert_eq!(first["model"], MODEL);
    assert_eq!(first["store"], false);
    assert_eq!(first["stream"], true);
    assert_eq!(first["include"], json!(["reasoning.encrypted_content"]));
    assert_offers_the_ask_tools(&first, &fixture.canonical_workspace());
    assert!(
        first["instructions"]
            .as_str()
            .expect("instructions")
            .starts_with("# Identity and context\n\n- You are oh-fx,")
    );
    assert_eq!(
        first["input"].as_array().expect("input").last(),
        Some(&json!({"role":"user","content":[{"type":"input_text","text":"Read README.md"}]}))
    );
    let replayed = requests[1].json()["input"]
        .as_array()
        .expect("input")
        .clone();
    let tail: Vec<&str> = replayed
        .iter()
        .rev()
        .take(4)
        .rev()
        .map(|item| item["type"].as_str().expect("item type"))
        .collect();
    assert_eq!(
        tail,
        [
            "reasoning",
            "message",
            "function_call",
            "function_call_output"
        ]
    );
    let reasoning = &replayed[replayed.len() - 4];
    assert_eq!(reasoning["encrypted_content"], "opaque-cipher");
    assert_eq!(replayed[replayed.len() - 3]["phase"], "commentary");
    assert_eq!(replayed[replayed.len() - 2]["call_id"], "call_1");
    assert_eq!(replayed[replayed.len() - 1]["call_id"], "call_1");
    assert!(
        replayed[replayed.len() - 1]["output"]
            .as_str()
            .expect("tool output")
            .contains("1\t# Readme")
    );

    let saved = fixture.saved();
    assert_eq!(saved["access_token"], FRESH_TOKEN);
    assert_eq!(saved["refresh_token"], ROTATED_REFRESH_TOKEN);
    assert_eq!(saved["account_id"], ACCOUNT);
    assert_eq!(mode(&fixture.credential_file()), 0o600);
    assert_eq!(mode(&fixture.paths.data), 0o700);

    let shown = format!("{debug}{seen:?}{report:?}");
    for secret in [
        SAVED_TOKEN,
        FRESH_TOKEN,
        REFRESH_TOKEN,
        ROTATED_REFRESH_TOKEN,
    ] {
        assert!(!shown.contains(secret), "{secret} leaked");
    }
}

#[tokio::test]
async fn codex_turns_carry_the_web_search_guidance_and_answer_its_calls_as_unavailable() {
    let fixture = Fixture::new();
    fixture.write_session(FAR_FUTURE_MS, 0o600);
    let auth = FakeServer::start([]);
    let search = events(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"web_search","arguments":""}}),
        json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"query\":\"zig news\"}"}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":5}}}),
    ]);
    let codex = FakeServer::start([Reply::sse(&search), Reply::sse(&text_events("Done."))]);
    let provider = fixture.provider(&auth, &codex).await.expect("provider");
    let mut agent = fixture.agent(provider);
    let (report, seen) = run(&mut agent, "What is new in Zig?").await;

    assert_eq!(report.outcome, TurnOutcome::Completed, "{report:?}");
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, UiEvent::ToolStarted { .. })),
        "{seen:?}"
    );
    let requests = codex.requests();
    assert_eq!(requests.len(), 2);
    let first = requests[0].json();
    assert_offers_the_ask_tools(&first, &fixture.canonical_workspace());
    let guidance = ofx_tools::WebSearch::default().spec().description.clone();
    let instructions = first["instructions"].as_str().expect("instructions");
    let (system, rest) = instructions
        .split_once(&format!("\n\n{guidance}\n\n"))
        .expect("the guidance follows the system prompt");
    assert_eq!(system, GATEWAY_SYSTEM_PROMPT);
    assert!(rest.starts_with("<fx-turn-context>\n"), "{rest}");
    let input = requests[1].json()["input"]
        .as_array()
        .expect("input")
        .clone();
    assert_eq!(
        input.last(),
        Some(&json!({
            "type": "function_call_output",
            "call_id": "call_1",
            "output": "web_search is unavailable: no local runtime with a configured Gateway transport policy is installed"
        }))
    );
}

#[tokio::test]
async fn a_subscription_signed_out_sends_nothing_with_its_login() {
    let fixture = Fixture::new();
    fixture.write_session(FAR_FUTURE_MS, 0o600);
    let auth = FakeServer::start([]);
    let codex = FakeServer::start([Reply::sse(&text_events("Never sent."))]);
    let catalog = FakeServer::start([]);
    let endpoints = SubscriptionEndpoints {
        models: CodexModelsEndpoints {
            models: format!("{}/backend-api/codex/models", catalog.base_url()),
            client_version: format!("{}/@openai/codex/latest", catalog.base_url()),
        },
        ..subscription_endpoints(&auth, &codex)
    };
    let subscription = fixture.subscription(endpoints).await.expect("subscription");
    let login = Arc::new(CodexLogin::default());
    login.sign_in(Arc::new(subscription));
    let provider = SubscriptionProvider::new(Arc::clone(&login));
    let mut agent = fixture.agent_on(Arc::new(provider), agent_config(MODEL, None, false));
    login.sign_out();
    let (report, _) = run(&mut agent, "Hello").await;
    assert_eq!(report.outcome, TurnOutcome::Failed, "{report:?}");
    assert_eq!(
        ModelSource::Codex(Arc::clone(&login)).catalog().await,
        ofx_contract::ModelCatalog::Failed { retry: None }
    );
    assert!(codex.requests().is_empty());
    assert!(catalog.requests().is_empty());
    assert!(auth.requests().is_empty());
}

#[tokio::test]
async fn an_unauthorized_reply_refreshes_the_login_once_and_replays_the_request() {
    let fixture = Fixture::new();
    fixture.write_session(FAR_FUTURE_MS, 0o600);
    let auth = FakeServer::start([token_reply(FRESH_TOKEN)]);
    let codex = FakeServer::start([
        Reply::status(401, r#"{"error":{"message":"token expired"}}"#),
        Reply::sse(&text_events("Recovered.")),
    ]);
    let provider = fixture.provider(&auth, &codex).await.expect("provider");
    let mut agent = fixture.agent(provider);
    let (report, _) = run(&mut agent, "Hello").await;

    assert_eq!(report.outcome, TurnOutcome::Completed, "{report:?}");
    assert_eq!(report.final_text, "Recovered.");
    assert_eq!(auth.requests().len(), 1);
    let requests = codex.requests();
    assert_eq!(requests.len(), 2);
    assert_codex_request(&requests[0], SAVED_TOKEN);
    assert_codex_request(&requests[1], FRESH_TOKEN);
    assert_eq!(requests[0].body, requests[1].body);
    assert_eq!(fixture.saved()["access_token"], FRESH_TOKEN);
}

#[tokio::test]
async fn a_cancelled_turn_stops_waiting_for_a_stalled_refresh_after_unauthorized() {
    let fixture = Fixture::new();
    fixture.write_session(FAR_FUTURE_MS, 0o600);
    let auth = FakeServer::start([Reply::held_status_with_headers(200, &[], "")]);
    let codex = FakeServer::start([Reply::status(
        401,
        r#"{"error":{"message":"token expired"}}"#,
    )]);
    let provider = fixture.provider(&auth, &codex).await.expect("provider");
    let mut agent = fixture.agent(provider);
    let cancel = CancellationToken::new();
    let interrupt = async {
        let started = Instant::now();
        while auth.requests().is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the turn never asked to refresh the login"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
        Instant::now()
    };
    let mut seen = Vec::new();
    let mut record = |event| seen.push(event);
    let turn = agent.run_turn("Hello", &mut record, &cancel);
    let (report, cancelled) = tokio::join!(turn, interrupt);

    assert!(cancelled.elapsed() < Duration::from_secs(10));
    assert_eq!(report.outcome, TurnOutcome::Interrupted, "{report:?}");
    assert_eq!(codex.requests().len(), 1);
    assert_eq!(auth.requests().len(), 1);
    assert_eq!(fixture.saved()["refresh_token"], REFRESH_TOKEN);
    assert!(!format!("{seen:?}").contains(REFRESH_TOKEN));
}

#[tokio::test]
async fn an_interactive_turn_cancelled_during_a_refresh_stops_while_the_refresh_saves() {
    let fixture = Fixture::new();
    fixture.write_session(FAR_FUTURE_MS, 0o600);
    let rotated = json!({
        "access_token": FRESH_TOKEN,
        "refresh_token": ROTATED_REFRESH_TOKEN,
        "expires_in": 3600,
    });
    let auth = FakeServer::start([Reply::delayed_status(
        200,
        rotated.to_string(),
        Duration::from_secs(3),
    )]);
    let codex = FakeServer::start([Reply::status(
        401,
        r#"{"error":{"message":"token expired"}}"#,
    )]);
    let refreshes = Arc::new(DetachedRefreshes::default());
    let provider = codex_subscription(
        Some(&fixture.paths),
        "oh-fx/test",
        subscription_endpoints(&auth, &codex),
        Some(Arc::clone(&refreshes)),
        &CancellationToken::new(),
    )
    .await
    .expect("provider")
    .provider;
    let mut agent = fixture.agent(provider);
    let cancel = CancellationToken::new();
    let interrupt = async {
        let started = Instant::now();
        while auth.requests().is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the turn never asked to refresh the login"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
        Instant::now()
    };
    let mut record = |_| {};
    let turn = agent.run_turn("Hello", &mut record, &cancel);
    let (report, cancelled) = tokio::join!(turn, interrupt);

    assert!(cancelled.elapsed() < Duration::from_secs(1));
    assert_eq!(report.outcome, TurnOutcome::Interrupted, "{report:?}");
    assert_eq!(fixture.saved()["refresh_token"], REFRESH_TOKEN);
    assert!(refreshes.pending());
    refreshes.settle().await;
    assert!(!refreshes.pending());
    assert_eq!(auth.requests().len(), 1);
    assert_eq!(fixture.saved()["access_token"], FRESH_TOKEN);
    assert_eq!(fixture.saved()["refresh_token"], ROTATED_REFRESH_TOKEN);
}

#[tokio::test]
async fn no_interactive_refresh_starts_once_the_exit_has_begun() {
    let fixture = Fixture::new();
    fixture.write_session(FAR_FUTURE_MS, 0o600);
    let auth = FakeServer::start([]);
    let codex = FakeServer::start([Reply::status(
        401,
        r#"{"error":{"message":"token expired"}}"#,
    )]);
    let refreshes = Arc::new(DetachedRefreshes::default());
    let provider = codex_subscription(
        Some(&fixture.paths),
        "oh-fx/test",
        subscription_endpoints(&auth, &codex),
        Some(Arc::clone(&refreshes)),
        &CancellationToken::new(),
    )
    .await
    .expect("provider")
    .provider;
    refreshes.close();
    let mut agent = fixture.agent(provider);
    let (report, _) = run(&mut agent, "Hello").await;

    assert_eq!(report.outcome, TurnOutcome::Failed, "{report:?}");
    assert_eq!(codex.requests().len(), 1);
    assert!(auth.requests().is_empty());
    assert!(!refreshes.wait_for_running());
    assert_eq!(fixture.saved()["refresh_token"], REFRESH_TOKEN);
}

#[tokio::test]
async fn a_rejected_refresh_after_unauthorized_retires_the_login() {
    let fixture = Fixture::new();
    fixture.write_session(FAR_FUTURE_MS, 0o600);
    let auth = FakeServer::start([Reply::status(
        400,
        r#"{"error":{"code":"refresh_token_expired"}}"#,
    )]);
    let codex = FakeServer::start([Reply::status(
        401,
        r#"{"error":{"message":"token expired"}}"#,
    )]);
    let provider = fixture.provider(&auth, &codex).await.expect("provider");
    let mut agent = fixture.agent(provider);
    let (report, seen) = run(&mut agent, "Hello").await;

    assert_eq!(report.outcome, TurnOutcome::Failed);
    let Some(TurnFailure::Provider(error)) = &report.failure else {
        panic!("expected a provider failure: {report:?}");
    };
    assert_eq!(error.status, Some(401));
    assert_eq!(error.kind, ProviderErrorKind::Unauthorized);
    assert_eq!(codex.requests().len(), 1);
    assert_eq!(auth.requests().len(), 1);
    assert!(!fixture.credential_file().exists());
    let shown = format!("{seen:?}{report:?}");
    assert!(!shown.contains(SAVED_TOKEN));
    assert!(!shown.contains(REFRESH_TOKEN));

    let unused = FakeServer::start([]);
    assert!(matches!(
        fixture.provider(&unused, &unused).await,
        Err(CodexUnavailable::MissingLogin)
    ));
}

#[tokio::test]
async fn usage_limits_wait_for_retry_after_and_then_recover() {
    let fixture = Fixture::new();
    fixture.write_session(FAR_FUTURE_MS, 0o600);
    let auth = FakeServer::start([]);
    let codex = FakeServer::start([
        Reply::status_with_headers(
            429,
            &[("Retry-After", "1")],
            r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached"}}"#,
        ),
        Reply::sse(&text_events("Later.")),
    ]);
    let provider = fixture.provider(&auth, &codex).await.expect("provider");
    let mut agent = fixture.agent(provider);
    let (report, seen) = run(&mut agent, "Hello").await;

    assert_eq!(report.outcome, TurnOutcome::Completed, "{report:?}");
    assert_eq!(report.final_text, "Later.");
    let retry = seen
        .iter()
        .find_map(|event| match event {
            UiEvent::Recovery { status, .. } if status.cause.is_some() => Some(status),
            _ => None,
        })
        .expect("a retry notice");
    assert_eq!(retry.cause, Some(ModelRecoveryCause::RateLimited));
    assert_eq!(retry.delay_seconds, 1);
    assert!(
        retry
            .label()
            .contains("HTTP 429 · usage_limit_reached: The usage limit has been reached")
    );
    assert_eq!(codex.requests().len(), 2);
    assert!(auth.requests().is_empty());
}

#[tokio::test]
async fn missing_expired_and_unsafe_logins_never_build_a_provider() {
    let unused = FakeServer::start([]);
    let missing = Fixture::new();
    assert!(matches!(
        missing.provider(&unused, &unused).await,
        Err(CodexUnavailable::MissingLogin)
    ));

    let expired = Fixture::new();
    expired.write_session(now_ms() - 1_000, 0o600);
    let auth = FakeServer::start([Reply::status(400, r#"{"error":"invalid_grant"}"#)]);
    assert!(matches!(
        expired.provider(&auth, &unused).await,
        Err(CodexUnavailable::MissingLogin)
    ));
    assert!(!expired.credential_file().exists());

    let readable = Fixture::new();
    readable.write_session(FAR_FUTURE_MS, 0o644);
    assert!(matches!(
        readable.provider(&unused, &unused).await,
        Err(CodexUnavailable::Preparation(
            PreparationError::CredentialStorageUnavailable
        ))
    ));

    assert!(matches!(
        codex_subscription(
            None,
            "oh-fx/test",
            SubscriptionEndpoints::default(),
            None,
            &CancellationToken::new()
        )
        .await,
        Err(CodexUnavailable::Preparation(
            PreparationError::CredentialStorageUnavailable
        ))
    ));
    assert!(unused.requests().is_empty());
}

mod capabilities;
mod reviews;
