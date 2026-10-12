use std::sync::Mutex;
use std::time::Duration;

use ofx_config::ContextLimits;
use ofx_contract::McpMenuSettled;
use ofx_mcp::{ConnectOptions, McpServerConfig, NativeConfigLoad, StartupPhase};

use super::super::super::{McpSources, lock};
use super::*;

const FEATURES: &str = r#"reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{},\"resources\":{},\"prompts\":{}},\"serverInfo\":{\"name\":\"docs\",\"version\":\"1\"}}" ;;
    *'"method":"tools/list"'*) reply "$id" '{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}' ;;
    *'"method":"resources/list"'*) reply "$id" '{"resources":[{"uri":"custom://alpha","name":"alpha","title":"Alpha"}]}' ;;
    *'"method":"resources/templates/list"'*) reply "$id" '{"resourceTemplates":[{"uriTemplate":"custom://project/{path}","name":"project","description":"Any file"}]}' ;;
    *'"method":"prompts/list"'*) reply "$id" '{"prompts":[{"name":"review","title":"Review","description":"Review code"}]}' ;;
  esac
done"#;

const TOOLS_ONLY: &str = r#"reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"plain\",\"version\":\"1\"}}" ;;
    *'"method":"tools/list"'*) reply "$id" '{"tools":[]}' ;;
  esac
done"#;

struct Fixture {
    host: McpHost,
    events: Arc<Mutex<Vec<UiEvent>>>,
    _home: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let script = |name: &str, body: &str| {
        McpServerConfig::stdio(name, "/bin/sh", vec!["-c".to_owned(), body.to_owned()])
    };
    let runtime = Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs: vec![script("docs", FEATURES), script("plain", TOOLS_ONLY)],
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        ContextLimits::default(),
    ));
    runtime.connect(StartupPhase::All).await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&events);
    let host = McpHost::new(
        runtime,
        McpSources::new(None, std::fs::canonicalize(home.path()).unwrap()),
        Arc::new(move |event| lock(&seen).push(event)),
    );
    Fixture {
        host,
        events,
        _home: home,
    }
}

impl Fixture {
    async fn next(&self, generation: u64) -> UiEvent {
        for _ in 0..1000 {
            let found = lock(&self.events).iter().find_map(|event| match event {
                UiEvent::McpMenuLoaded { generation: g, .. } if *g == generation => {
                    Some(event.clone())
                }
                UiEvent::McpMenuSettled { settled, .. } if settled.generation == generation => {
                    Some(event.clone())
                }
                _ => None,
            });
            if let Some(event) = found {
                return event;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no result for {generation}: {:?}", lock(&self.events));
    }

    async fn catalog(&self, generation: u64, request: McpMenuCatalogRequest) -> McpMenuCatalog {
        self.host.load_menu_catalog(generation, request);
        match self.next(generation).await {
            UiEvent::McpMenuLoaded { catalog, .. } => catalog,
            event => panic!("expected a catalog: {event:?}"),
        }
    }
}

#[tokio::test]
async fn catalogs_list_every_tool_and_the_servers_resources_and_prompts() {
    let fixture = fixture().await;
    assert_eq!(
        fixture.catalog(1, McpMenuCatalogRequest::Tools).await,
        McpMenuCatalog::Tools(vec!["mcp_docs_echo".to_owned()])
    );
    assert_eq!(
        fixture
            .catalog(2, McpMenuCatalogRequest::Resources("docs".to_owned()))
            .await,
        McpMenuCatalog::Resources(vec![
            McpMenuResource {
                uri: "custom://alpha".to_owned(),
                name: "alpha".to_owned(),
                title: Some("Alpha".to_owned()),
                description: None,
                template: false,
            },
            McpMenuResource {
                uri: "custom://project/{path}".to_owned(),
                name: "project".to_owned(),
                title: None,
                description: Some("Any file".to_owned()),
                template: true,
            },
        ])
    );
    assert_eq!(
        fixture
            .catalog(3, McpMenuCatalogRequest::Prompts("docs".to_owned()))
            .await,
        McpMenuCatalog::Prompts(vec![McpMenuPrompt {
            name: "review".to_owned(),
            title: Some("Review".to_owned()),
            description: Some("Review code".to_owned()),
        }])
    );
}

#[tokio::test]
async fn unsupported_catalogs_are_empty_and_other_failures_settle_the_menu() {
    let fixture = fixture().await;
    assert_eq!(
        fixture
            .catalog(1, McpMenuCatalogRequest::Resources("plain".to_owned()))
            .await,
        McpMenuCatalog::Resources(Vec::new())
    );
    assert_eq!(
        fixture
            .catalog(2, McpMenuCatalogRequest::Prompts("plain".to_owned()))
            .await,
        McpMenuCatalog::Prompts(Vec::new())
    );
    fixture
        .host
        .load_menu_catalog(3, McpMenuCatalogRequest::Prompts("absent".to_owned()));
    let UiEvent::McpMenuSettled { settled, .. } = fixture.next(3).await else {
        panic!("expected a failed settlement");
    };
    assert_eq!(
        settled,
        McpMenuSettled {
            generation: 3,
            outcome: McpMenuOutcome::Failed,
            feedback: "MCP catalog failed: McpServerNotFound".to_owned(),
        }
    );
}

#[tokio::test]
async fn only_a_cancel_for_the_pending_generation_discards_it() {
    let fixture = fixture().await;
    fixture
        .host
        .load_menu_catalog(4, McpMenuCatalogRequest::Tools);
    fixture.host.cancel_menu_operation(3, McpMenuCancel::Back);
    assert!(lock(&fixture.host.menu_operation).is_some());
    fixture.host.cancel_menu_operation(4, McpMenuCancel::Closed);
    assert!(lock(&fixture.host.menu_operation).is_none());
}
