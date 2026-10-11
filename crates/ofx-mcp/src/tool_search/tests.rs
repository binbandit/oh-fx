use std::path::Path;

use ofx_config::{ContextLimits, parse_context_limit_override};
use ofx_contract::{McpSearchHost, McpSearchRequest, McpToolSearch};
use serde_json::json;

use super::*;
use crate::mcp_contract::{EnvVar, McpServerConfig};
use crate::mcp_runtime::McpRuntime;
use crate::native_config::NativeConfigLoad;
use crate::server_transport::ConnectOptions;
use crate::startup_admission::StartupPhase;

const SERVER: &str = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      while [ -f "$STATE/hold" ]; do sleep 0.05; done
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"}$(cat "$STATE/$NAME.instructions" 2>/dev/null)}" ;;
    *'"method":"tools/list"'*) reply "$id" "$(cat "$STATE/$NAME.tools")" ;;
  esac
done
"#;
const DATADOG: &[(&str, &str)] = &[
    ("list_monitors", "List Datadog monitors and incidents"),
    ("get_dashboard", "Fetch a dashboard"),
];
const LISTED: &str = r#"{"tools":[{"name":"mcp_datadog_list_monitors","server":"datadog","description":"List Datadog monitors and incidents","purpose":"List Datadog monitors and incidents","usage":["mcp","datadog","list_monitors"]},{"name":"mcp_datadog_get_dashboard","server":"datadog","description":"Fetch a dashboard","purpose":"Fetch a dashboard","usage":["mcp","datadog","get_dashboard"]}],"count":2,"total_matches":2,"more_available":false,"next_cursor":null}"#;

struct Fixture {
    state: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            state: tempfile::tempdir().unwrap(),
        }
    }

    fn server(&self, name: &str, tools: &[(&str, &str)]) -> McpServerConfig {
        let tools: Vec<_> = tools
            .iter()
            .map(|(tool, description)| {
                let mut tool = json!({"name": tool, "inputSchema": {"type": "object"}});
                if !description.is_empty() {
                    tool["description"] = json!(description);
                }
                tool
            })
            .collect();
        self.listing(name, &json!({ "tools": tools }))
    }

    fn listing(&self, name: &str, listing: &Value) -> McpServerConfig {
        std::fs::write(
            self.path().join(format!("{name}.tools")),
            listing.to_string(),
        )
        .unwrap();
        let mut config =
            McpServerConfig::stdio(name, "/bin/sh", vec!["-c".to_owned(), SERVER.to_owned()]);
        for (key, value) in [("STATE", self.path().to_str().unwrap()), ("NAME", name)] {
            config.env.push(EnvVar {
                key: key.to_owned(),
                value: value.to_owned(),
            });
        }
        config
    }

    fn instructions(&self, name: &str, text: &str) {
        std::fs::write(
            self.path().join(format!("{name}.instructions")),
            format!(",\"instructions\":{}", json!(text)),
        )
        .unwrap();
    }

    fn path(&self) -> &Path {
        self.state.path()
    }
}

fn failing(name: &str) -> McpServerConfig {
    McpServerConfig::stdio(name, "/bin/sh", vec!["-c".to_owned(), "exit 3".to_owned()])
}

fn runtime(configs: Vec<McpServerConfig>, overrides: &[&str]) -> McpRuntime {
    let mut limits = ContextLimits::default();
    let overrides: Vec<_> = overrides
        .iter()
        .map(|text| parse_context_limit_override(text.as_bytes()).unwrap())
        .collect();
    limits.apply_command_line(&overrides);
    McpRuntime::new(
        NativeConfigLoad {
            configs,
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        limits,
    )
}

async fn connected(configs: Vec<McpServerConfig>, overrides: &[&str]) -> McpRuntime {
    let runtime = runtime(configs, overrides);
    runtime.connect(StartupPhase::All).await;
    runtime
}

fn search(runtime: &McpRuntime, query: &str, server: Option<&str>) -> McpSearchResult {
    search_from(runtime, query, server, McpSearchHost::Interactive)
}

fn search_within(
    runtime: &McpRuntime,
    query: &str,
    server: Option<&str>,
    result_bytes: usize,
) -> McpSearchResult {
    search_from(runtime, query, server, McpSearchHost::Ask { result_bytes })
}

fn search_from(
    runtime: &McpRuntime,
    query: &str,
    server: Option<&str>,
    host: McpSearchHost,
) -> McpSearchResult {
    runtime.search(&McpSearchRequest {
        query: Arc::new(PreparedQuery::prepare(query.to_owned()).unwrap()),
        server: server.map(str::to_owned),
        host,
    })
}

#[tokio::test]
async fn a_named_server_ranks_all_of_its_tools_with_their_tags_and_purpose() {
    let fixture = Fixture::new();
    let runtime = connected(
        vec![
            fixture.server("datadog", DATADOG),
            fixture.server("other", &[("list_tools", "Return tools from a server")]),
        ],
        &[],
    )
    .await;
    let result = search(&runtime, "datadog monitor incidents", None);
    assert_eq!(result.model_output, LISTED);
    assert_eq!(result.notice, None);
    assert_eq!(
        search(&runtime, "datadog monitor incidents", Some("datadog")).model_output,
        LISTED
    );
}

#[tokio::test]
async fn a_query_without_clear_intent_matches_nothing() {
    let fixture = Fixture::new();
    let runtime = connected(vec![fixture.server("datadog", DATADOG)], &[]).await;
    assert_eq!(
        search(&runtime, "calendar", None).model_output,
        r#"{"tools":[],"count":0,"total_matches":0,"more_available":false,"next_cursor":null}"#
    );
}

#[tokio::test]
async fn a_server_scope_names_an_unknown_or_failed_server() {
    let fixture = Fixture::new();
    let runtime = connected(
        vec![fixture.server("datadog", DATADOG), failing("broken")],
        &[],
    )
    .await;
    assert_eq!(
        search(&runtime, "monitors", Some("absent")).model_output,
        SERVER_NOT_FOUND
    );
    let Lifecycle::Failed(failure) = runtime_server(&runtime, "broken").lifecycle() else {
        panic!("the server failed");
    };
    assert_eq!(
        search(&runtime, "monitors", Some("broken")).model_output,
        format!(
            r#"{{"tools":[],"count":0,"total_matches":0,"more_available":false,"next_cursor":null,"state":"server_failed","error":{}}}"#,
            encoded_json(&format!("MCP server 'broken' is unavailable: {failure}"))
        )
    );
    assert_eq!(
        search(&runtime, "broken", None).model_output,
        r#"{"tools":[],"count":0,"total_matches":0,"more_available":false,"next_cursor":null}"#
    );
}

fn runtime_server(runtime: &McpRuntime, name: &str) -> Arc<Server> {
    runtime
        .current()
        .into_iter()
        .find(|server| server.config.name == name)
        .unwrap()
}

#[tokio::test]
async fn a_failed_server_missing_its_bearer_token_asks_for_the_environment_variable() {
    let variable = "OH_FX_TOOL_SEARCH_UNSET_TOKEN";
    assert!(std::env::var_os(variable).is_none());
    let mut broken = failing("broken");
    broken.bearer_token_env = Some(variable.to_owned());
    let runtime = connected(vec![broken], &[]).await;
    let expected = format!(
        r#"{{"tools":[],"count":0,"authentication_required":{{"server":"broken","interactive":false,"environment":"{variable}","message":"Set this environment variable before starting oh-fx."}}}}"#
    );
    assert_eq!(
        search(&runtime, "use (broken) now", None).model_output,
        expected
    );
    assert_eq!(
        search(&runtime, "anything", Some("broken")).model_output,
        expected
    );
    assert_ne!(
        search(&runtime, "broken-ish tools", None).model_output,
        expected
    );
}

#[tokio::test]
async fn authentication_guidance_needs_an_observed_challenge() {
    let remote = |name: &str| McpServerConfig {
        auth: Some(crate::mcp_contract::McpAuthConfig::default()),
        ..McpServerConfig::remote(
            name,
            crate::mcp_contract::TransportType::Http,
            "https://mcp.example/mcp",
        )
    };
    let runtime = runtime(vec![remote("plain"), remote("slack"), remote("a/b")], &[]);
    let guidance = |name: &str| {
        format!(
            r#"{{"tools":[],"count":0,"authentication_required":{{"server":"{name}","interactive":true,"message":"Run /mcp auth {name} --open in an interactive oh-fx session."}}}}"#
        )
    };
    assert_ne!(
        search(&runtime, "plain", None).model_output,
        guidance("plain")
    );
    for name in ["plain", "slack", "a/b"] {
        runtime_server(&runtime, name)
            .auth
            .store_pending(crate::mcp_auth::Challenge::default());
    }
    assert_eq!(
        search(&runtime, "plain", None).model_output,
        guidance("plain")
    );
    assert_eq!(
        search(&runtime, "anything", Some("plain")).model_output,
        guidance("plain")
    );
    assert_eq!(
        search(&runtime, "slack data", None).model_output,
        guidance("slack")
    );
    assert_eq!(
        search(&runtime, "authenticate a/b now", None).model_output,
        guidance("a/b")
    );
    assert_ne!(
        search(&runtime, "authenticate xa/by now", None).model_output,
        guidance("a/b")
    );
}

#[tokio::test]
async fn oauth_guidance_needs_a_challenge_and_comes_before_the_bearer_guidance() {
    let variable = "OH_FX_TOOL_SEARCH_UNSET_TOKEN";
    assert!(std::env::var_os(variable).is_none());
    let mut broken = failing("broken");
    broken.bearer_token_env = Some(variable.to_owned());
    broken.auth = Some(crate::mcp_contract::McpAuthConfig::default());
    let runtime = connected(vec![broken], &[]).await;
    assert_eq!(
        search(&runtime, "broken", None).model_output,
        format!(
            r#"{{"tools":[],"count":0,"authentication_required":{{"server":"broken","interactive":false,"environment":"{variable}","message":"Set this environment variable before starting oh-fx."}}}}"#
        )
    );
    runtime_server(&runtime, "broken")
        .auth
        .store_pending(crate::mcp_auth::Challenge::default());
    assert_eq!(
        search(&runtime, "broken", None).model_output,
        r#"{"tools":[],"count":0,"authentication_required":{"server":"broken","interactive":true,"message":"Run /mcp auth broken --open in an interactive oh-fx session."}}"#
    );
}

#[tokio::test]
async fn an_oversized_result_omits_trailing_tools_with_a_cursor_and_a_notice() {
    let fixture = Fixture::new();
    let long = format!("List Datadog monitors {}", "x".repeat(300));
    let runtime = connected(
        vec![fixture.server(
            "datadog",
            &[("list_monitors", &long), ("get_dashboard", &long)],
        )],
        &[],
    )
    .await;
    let full = search(&runtime, "datadog", None);
    assert_eq!(full.notice, None);
    let limit = full.model_output.len() - 1;
    let result = search_within(&runtime, "datadog", None, limit);
    let output: Value = serde_json::from_str(&result.model_output).unwrap();
    assert_eq!(output["count"], 1);
    assert_eq!(output["total_matches"], 2);
    assert_eq!(output["more_available"], true);
    let cursor = output["next_cursor"].as_str().unwrap();
    assert!(
        cursor.starts_with("c1:m:") && cursor.ends_with(":1"),
        "{cursor}"
    );
    assert_eq!(output["tools"][0]["name"], "mcp_datadog_get_dashboard");
    assert_eq!(
        output["context_limit"],
        json!({
            "name": "mcp_search_result_bytes",
            "action": "omitted",
            "omitted_count": 1,
            "observed_bytes": full.model_output.len(),
            "effective_bytes": limit,
            "source": "compiled default",
            "override": "--context-limit mcp_search_result_bytes=BYTES|off"
        })
    );
    assert!(result.model_output.len() <= limit);
    assert_eq!(
        result.notice.unwrap(),
        format!(
            "[context] MCP search omitted 1 tool(s) (mcp_datadog_list_monitors): observed={} bytes effective={limit} bytes source=compiled default; override with --context-limit mcp_search_result_bytes=BYTES|off",
            full.model_output.len()
        )
    );
    assert_eq!(
        search_within(&runtime, "datadog", None, 1 << 20).model_output,
        full.model_output
    );
    let tiny = search_within(&runtime, "datadog", None, 1);
    let output: Value = serde_json::from_str(&tiny.model_output).unwrap();
    assert_eq!(output["count"], 0);
    assert_eq!(output["context_limit"]["omitted_count"], 2);
    assert!(tiny.notice.unwrap().starts_with(
        "[context] MCP search omitted 2 tool(s) (mcp_datadog_get_dashboard, mcp_datadog_list_monitors): "
    ));
}

#[tokio::test]
async fn long_descriptions_are_cut_on_encoded_boundaries_and_reported() {
    let fixture = Fixture::new();
    let runtime = connected(
        vec![fixture.server("notes", &[("first", "a & b <c> notes"), ("second", "")])],
        &["mcp_description_bytes=4"],
    )
    .await;
    let result = search(&runtime, "notes", None);
    let output: Value = serde_json::from_str(&result.model_output).unwrap();
    assert_eq!(output["tools"][0]["name"], "mcp_notes_first");
    assert_eq!(output["tools"][0]["description"], "a ");
    assert_eq!(output["tools"][0]["purpose"], "a ");
    assert_eq!(
        output["tools"][0]["context_limit"],
        json!({
            "name": "mcp_description_bytes",
            "action": "truncated",
            "observed_bytes": 25,
            "effective_bytes": 4,
            "source": "command line",
            "override": "--context-limit mcp_description_bytes=BYTES|off"
        })
    );
    assert_eq!(output["tools"][1]["description"], "MCP ");
    assert_eq!(
        result.notice.unwrap(),
        "[context] MCP description for \"mcp_notes_first\" truncated: observed=25 bytes effective=4 bytes source=command line; override with --context-limit mcp_description_bytes=BYTES|off\n[context] MCP description for \"mcp_notes_second\" truncated: observed=8 bytes effective=4 bytes source=command line; override with --context-limit mcp_description_bytes=BYTES|off\n"
    );
}

#[tokio::test]
async fn a_tool_without_a_description_is_described_as_an_mcp_tool() {
    let fixture = Fixture::new();
    let runtime = connected(vec![fixture.server("notes", &[("blank", "")])], &[]).await;
    let output: Value =
        serde_json::from_str(&search(&runtime, "notes", None).model_output).unwrap();
    assert_eq!(output["tools"][0]["description"], "MCP tool");
}

#[tokio::test]
async fn matched_schemas_are_loaded_within_the_selected_schema_budget() {
    let fixture = Fixture::new();
    let config = fixture.server("datadog", DATADOG);
    let length = |name: &str, tool: &str, description: &str| {
        let tool = Tool {
            name: tool.to_owned(),
            title: None,
            description: description.to_owned(),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            icons: None,
            annotations: None,
            meta: None,
        };
        match project(
            name,
            &tool,
            None,
            SchemaLimits::from(&ContextLimits::default()),
        ) {
            Projection::Selected { spec, .. } => selected_schema(&spec).len(),
            Projection::Rejected { .. } => unreachable!(),
        }
    };
    let first = length("mcp_datadog_list_monitors", DATADOG[0].0, DATADOG[0].1);
    let second = length("mcp_datadog_get_dashboard", DATADOG[1].0, DATADOG[1].1);
    assert!(second < first);
    let budget = format!("mcp_selected_schema_bytes={}", first + 5);
    let runtime = connected(vec![config.clone()], &[&budget]).await;
    let budgeted = search(&runtime, "datadog", None);
    assert_eq!(budgeted.notice.as_deref(), Some(BUDGET_NOTICE));
    assert_eq!(budgeted.selected, ["mcp_datadog_list_monitors"]);
    let runtime = connected(
        vec![config.clone()],
        &[&format!("mcp_selected_schema_bytes={second}")],
    )
    .await;
    assert_eq!(
        search(&runtime, "datadog", None).notice.unwrap(),
        format!(
            "[context] MCP schema \"mcp_datadog_list_monitors\" rejected: observed={first} bytes effective={second} bytes source=command line; override with --context-limit mcp_selected_schema_bytes=BYTES|off"
        )
    );
    let runtime = connected(
        vec![config],
        &[&format!("mcp_selected_schema_bytes={}", first + second)],
    )
    .await;
    let loaded = search(&runtime, "datadog", None);
    assert_eq!(loaded.notice, None);
    assert_eq!(
        loaded.selected,
        ["mcp_datadog_list_monitors", "mcp_datadog_get_dashboard"]
    );
}

#[tokio::test]
async fn truncated_server_instructions_are_reported_for_each_loaded_schema() {
    let fixture = Fixture::new();
    let config = fixture.server("datadog", DATADOG);
    fixture.instructions("datadog", "Prefer monitors.\nKeep dashboards small.");
    let runtime = connected(vec![config], &["mcp_server_instructions_bytes=20"]).await;
    let notice = search(&runtime, "datadog", None).notice.unwrap();
    let lines: Vec<_> = notice.lines().collect();
    assert_eq!(
        lines,
        [
            "[context] MCP schema \"mcp_datadog_list_monitors\" instructions truncated: observed=39 bytes effective=20 bytes source=command line; override with --context-limit mcp_server_instructions_bytes=BYTES|off",
            "[context] MCP schema \"mcp_datadog_get_dashboard\" instructions truncated: observed=39 bytes effective=20 bytes source=command line; override with --context-limit mcp_server_instructions_bytes=BYTES|off",
        ]
    );
}

#[tokio::test]
async fn server_instructions_are_searched_as_secondary_evidence() {
    let fixture = Fixture::new();
    let ops = fixture.server("ops", &[("run", "Run a job"), ("stop", "Stop a job")]);
    fixture.instructions("ops", "Handles kubernetes rollouts.");
    let other = fixture.server("other", &[("list", "List things")]);
    let runtime = connected(vec![ops, other], &[]).await;
    let output: Value =
        serde_json::from_str(&search(&runtime, "kubernetes rollouts", None).model_output).unwrap();
    let names: Vec<_> = output["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["mcp_ops_run", "mcp_ops_stop"]);
}

#[tokio::test]
async fn a_search_during_startup_discovery_with_no_ready_server_reports_discovering() {
    let fixture = Fixture::new();
    std::fs::write(fixture.path().join("hold"), "").unwrap();
    let runtime = runtime(vec![fixture.server("datadog", DATADOG)], &[]);
    let discovery = runtime.connect(StartupPhase::All);
    assert_eq!(
        search(&runtime, "datadog", None).model_output,
        r#"{"tools":[],"count":0,"state":"discovering","retryable":true}"#
    );
    std::fs::remove_file(fixture.path().join("hold")).unwrap();
    discovery.await;
    assert_eq!(search(&runtime, "datadog", None).model_output, LISTED);
}

#[tokio::test]
async fn only_the_leading_bytes_of_descriptions_and_schemas_are_searched() {
    let fixture = Fixture::new();
    let description = format!(
        "zearly123 {} zlate987",
        "x".repeat(DESCRIPTION_SEARCH_BYTES)
    );
    let padding = format!("{} zlate654", "x".repeat(SCHEMA_SEARCH_BYTES));
    let bounded = fixture.listing(
        "bounded",
        &json!({"tools": [{
            "name": "probe",
            "description": description,
            "inputSchema": {"type": "object", "properties": {"zearly456": {"type": "string"}, "padding": {"description": padding}}}
        }]}),
    );
    let other = fixture.server("other", &[("list", "List things")]);
    let runtime = connected(vec![bounded, other], &[]).await;
    for (query, found) in [
        ("zearly123 zearly456", true),
        ("zlate987 zearly456", false),
        ("zearly123 zlate654", false),
        ("zlate987 zlate654", false),
    ] {
        assert_eq!(
            search(&runtime, query, None)
                .model_output
                .contains("mcp_bounded_probe"),
            found,
            "{query}"
        );
    }
}

#[test]
fn a_candidate_keeps_only_the_searched_schema_prefix_on_a_character_boundary() {
    let schema = |text: String| Tool {
        name: "probe".to_owned(),
        title: None,
        description: String::new(),
        input_schema: json!({ "d": text }),
        output_schema: None,
        icons: None,
        annotations: None,
        meta: None,
    };
    let opening = r#"{"d":""#.len();
    let straddling = schema(format!(
        "{}{}",
        "x".repeat(SCHEMA_SEARCH_BYTES - 1 - opening),
        "\u{e9}".repeat(8)
    ));
    let full = straddling.input_schema.to_string();
    assert!(!full.is_char_boundary(SCHEMA_SEARCH_BYTES));
    let stored = searchable_schema(&straddling);
    assert_eq!(stored, &full[..SCHEMA_SEARCH_BYTES - 1]);
    assert!(stored.capacity() < SCHEMA_SEARCH_BYTES);
    let short = schema("brief".to_owned());
    assert_eq!(searchable_schema(&short), short.input_schema.to_string());
}

#[tokio::test]
async fn dropping_or_abandoning_startup_discovery_ends_the_discovering_state() {
    let fixture = Fixture::new();
    std::fs::write(fixture.path().join("hold"), "").unwrap();
    let runtime = runtime(vec![fixture.server("datadog", DATADOG)], &[]);
    let empty =
        r#"{"tools":[],"count":0,"total_matches":0,"more_available":false,"next_cursor":null}"#;
    drop(runtime.connect(StartupPhase::All));
    assert_eq!(search(&runtime, "datadog", None).model_output, empty);
    let discovery = runtime.connect(StartupPhase::All);
    assert!(
        search(&runtime, "datadog", None)
            .model_output
            .contains("discovering")
    );
    discovery.abandon().await;
    assert_eq!(search(&runtime, "datadog", None).model_output, empty);
}

#[tokio::test]
async fn a_search_lists_an_expired_tool_list_again_first() {
    let fixture = Fixture::new();
    let listing = |tools: &[(&str, &str)]| {
        json!({
            "tools": tools
                .iter()
                .map(|(name, description)| json!({"name": name, "description": description, "inputSchema": {"type": "object"}}))
                .collect::<Vec<_>>(),
            "ttlMs": 1
        })
    };
    let config = fixture.listing("datadog", &listing(&[DATADOG[1]]));
    let runtime = Arc::new(connected(vec![config], &[]).await);
    fixture.listing("datadog", &listing(DATADOG));
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let request = McpSearchRequest {
        query: Arc::new(PreparedQuery::prepare("datadog".to_owned()).unwrap()),
        server: None,
        host: McpSearchHost::Interactive,
    };
    assert_eq!(
        McpToolSearch::search_tools(Arc::clone(&runtime), request)
            .await
            .model_output,
        LISTED
    );
}

async fn ask_search(runtime: &Arc<McpRuntime>, query: &str, server: Option<&str>) -> String {
    McpToolSearch::search_tools(
        Arc::clone(runtime),
        McpSearchRequest {
            query: Arc::new(PreparedQuery::prepare(query.to_owned()).unwrap()),
            server: server.map(str::to_owned),
            host: McpSearchHost::Ask {
                result_bytes: 16384,
            },
        },
    )
    .await
    .model_output
}

fn started(runtime: &McpRuntime, name: &str) -> bool {
    !matches!(runtime_server(runtime, name).lifecycle(), Lifecycle::Idle)
}

fn availability(runtime: &McpRuntime) -> Vec<(String, crate::model_catalog::Availability)> {
    runtime
        .model_catalog()
        .into_iter()
        .map(|server| (server.name, server.availability))
        .collect()
}

#[tokio::test]
async fn ask_starts_optional_servers_only_when_a_search_needs_them() {
    use crate::model_catalog::Availability::{AvailableOnDemand, Ready};
    let fixture = Fixture::new();
    let mut docs = fixture.server("docs", &[("read", "Read docs")]);
    docs.required = true;
    let runtime = Arc::new(runtime(
        vec![
            docs,
            fixture.server("datadog", DATADOG),
            fixture.server("other", &[("list", "List things")]),
        ],
        &[],
    ));
    runtime.connect_for_ask(false).await;
    assert!(started(&runtime, "docs"));
    assert!(!started(&runtime, "datadog") && !started(&runtime, "other"));
    assert_eq!(
        availability(&runtime),
        [
            ("docs".to_owned(), Ready),
            ("datadog".to_owned(), AvailableOnDemand),
            ("other".to_owned(), AvailableOnDemand),
        ]
    );
    assert_eq!(
        ask_search(&runtime, "monitors", Some("absent")).await,
        r#"{"tools":[],"count":0,"error":"McpServerNotFound"}"#
    );
    assert_eq!(
        ask_search(&runtime, "datadog monitor incidents", Some("datadog")).await,
        LISTED
    );
    assert!(started(&runtime, "datadog"));
    assert!(!started(&runtime, "other"));
    let output: Value = serde_json::from_str(&ask_search(&runtime, "other", None).await).unwrap();
    assert_eq!(output["tools"][0]["name"], "mcp_other_list");
    assert!(started(&runtime, "other"));
    assert_eq!(
        availability(&runtime),
        [
            ("docs".to_owned(), Ready),
            ("datadog".to_owned(), Ready),
            ("other".to_owned(), Ready),
        ]
    );
}

#[tokio::test]
async fn ask_reports_a_server_that_fails_to_start_for_a_scoped_search() {
    let runtime = Arc::new(runtime(vec![failing("broken")], &[]));
    runtime.connect_for_ask(false).await;
    assert!(!started(&runtime, "broken"));
    let output: Value =
        serde_json::from_str(&ask_search(&runtime, "anything", Some("broken")).await).unwrap();
    assert_eq!(output["state"], "server_failed");
    assert!(
        output["error"]
            .as_str()
            .unwrap()
            .starts_with("MCP server 'broken' is unavailable: MCP server exited with code 3")
    );
}

#[tokio::test]
async fn an_unscoped_ask_search_dropped_while_servers_start_records_them_as_cancelled() {
    let fixture = Fixture::new();
    std::fs::write(fixture.path().join("hold"), "").unwrap();
    let runtime = Arc::new(runtime(vec![fixture.server("datadog", DATADOG)], &[]));
    runtime.connect_for_ask(false).await;
    let searching = tokio::spawn({
        let runtime = Arc::clone(&runtime);
        async move { ask_search(&runtime, "datadog", None).await }
    });
    let settled = |wanted: fn(&Lifecycle) -> bool| {
        let runtime = Arc::clone(&runtime);
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !wanted(&runtime_server(&runtime, "datadog").lifecycle()) {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
    };
    settled(|lifecycle| matches!(lifecycle, Lifecycle::Starting)).await;
    searching.abort();
    assert!(searching.await.unwrap_err().is_cancelled());
    settled(|lifecycle| matches!(lifecycle, Lifecycle::Failed(failure) if failure == "Cancelled"))
        .await;
    std::fs::remove_file(fixture.path().join("hold")).unwrap();
    let output: Value = serde_json::from_str(
        &tokio::time::timeout(
            std::time::Duration::from_secs(10),
            ask_search(&runtime, "datadog", Some("datadog")),
        )
        .await
        .expect("a later search answers at once"),
    )
    .unwrap();
    assert_eq!(output["state"], "server_failed");
    assert_eq!(
        output["error"],
        "MCP server 'datadog' is unavailable: Cancelled"
    );
}

#[tokio::test]
async fn a_terminal_ask_starts_every_server_before_the_turn() {
    let fixture = Fixture::new();
    let runtime = Arc::new(runtime(vec![fixture.server("datadog", DATADOG)], &[]));
    runtime.connect_for_ask(true).await;
    assert!(started(&runtime, "datadog"));
    assert_eq!(
        ask_search(&runtime, "monitors", Some("absent")).await,
        r#"{"tools":[],"count":0,"error":"McpServerNotFound"}"#
    );
}

#[tokio::test]
async fn the_shell_never_starts_a_server_for_a_search() {
    let fixture = Fixture::new();
    let runtime = Arc::new(runtime(vec![fixture.server("datadog", DATADOG)], &[]));
    let output = McpToolSearch::search_tools(
        Arc::clone(&runtime),
        McpSearchRequest {
            query: Arc::new(PreparedQuery::prepare("datadog".to_owned()).unwrap()),
            server: Some("absent".to_owned()),
            host: McpSearchHost::Interactive,
        },
    )
    .await;
    assert_eq!(output.model_output, SERVER_NOT_FOUND);
    assert!(!started(&runtime, "datadog"));
    assert_eq!(
        availability(&runtime),
        [(
            "datadog".to_owned(),
            crate::model_catalog::Availability::Unavailable
        )]
    );
}
