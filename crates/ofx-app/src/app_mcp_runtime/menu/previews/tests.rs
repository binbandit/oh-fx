use std::sync::Mutex;
use std::time::Duration;

use ofx_config::{ContextLimitName, ContextLimitOverride, ContextLimitValue, ContextLimits};
use ofx_contract::{McpMenuOutcome, McpMenuSettled};
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
    *'"method":"tools/list"'*) reply "$id" '{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object"}},{"name":"wide","inputSchema":{"type":"object","properties":{"first":{"type":"string","description":"a long description that does not fit"}}}}]}' ;;
    *'"method":"resources/list"'*) reply "$id" '{"resources":[{"uri":"custom://alpha","name":"alpha"},{"uri":"custom://broken","name":"broken"}]}' ;;
    *'"method":"resources/templates/list"'*) reply "$id" '{"resourceTemplates":[]}' ;;
    *'"method":"prompts/list"'*) reply "$id" '{"prompts":[{"name":"review"},{"name":"tone","arguments":[{"name":"tone","required":true}]}]}' ;;
    *'custom://broken'*) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32603,"message":"broken"}}\n' "$id" ;;
    *'"method":"resources/read"'*) reply "$id" '{"contents":[{"uri":"custom://alpha","text":"line one\nline two"},{"uri":"custom://alpha","blob":"AAAA"}]}' ;;
    *'"method":"prompts/get"'*'"arguments":{"tone":"warm"}'*) reply "$id" '{"messages":[{"role":"user","content":{"type":"text","text":"Warm review"}}]}' ;;
    *'"method":"prompts/get"'*) reply "$id" '{"messages":[{"role":"user","content":{"type":"text","text":"Review this"}},{"role":"assistant","content":{"type":"text","text":"Sure"}}]}' ;;
  esac
done"#;

struct Fixture {
    host: McpHost,
    events: Arc<Mutex<Vec<UiEvent>>>,
    _home: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let mut limits = ContextLimits::default();
    limits.apply_command_line(&[ContextLimitOverride {
        name: ContextLimitName::McpSelectedSchemaBytes,
        value: ContextLimitValue::Bytes(160),
    }]);
    let home = tempfile::tempdir().unwrap();
    let runtime = Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs: vec![McpServerConfig::stdio(
                "docs",
                "/bin/sh",
                vec!["-c".to_owned(), FEATURES.to_owned()],
            )],
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        limits,
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
    async fn result(&self, generation: u64, target: McpMenuPreviewTarget) -> UiEvent {
        self.host.load_menu_preview(generation, target);
        for _ in 0..1000 {
            let found = lock(&self.events).iter().find_map(|event| match event {
                UiEvent::McpMenuPreviewed { generation: g, .. } if *g == generation => {
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

    async fn preview(&self, generation: u64, target: McpMenuPreviewTarget) -> McpMenuPreview {
        match self.result(generation, target).await {
            UiEvent::McpMenuPreviewed { preview, .. } => preview,
            event => panic!("expected a preview: {event:?}"),
        }
    }

    async fn feedback(&self, generation: u64, target: McpMenuPreviewTarget) -> String {
        self.failure(generation, target).await.feedback
    }

    async fn failure(&self, generation: u64, target: McpMenuPreviewTarget) -> McpMenuSettled {
        match self.result(generation, target).await {
            UiEvent::McpMenuSettled { settled, .. } => settled,
            event => panic!("expected a failure: {event:?}"),
        }
    }
}

#[tokio::test]
async fn previews_show_a_tool_schema_a_resource_and_a_prompt_with_upstreams_headings() {
    let fixture = fixture().await;
    let schema = r#"{"type":"function","name":"mcp_docs_echo","description":"Echo text","inputSchema":{"type":"object"}}"#;
    assert_eq!(
        fixture
            .preview(1, McpMenuPreviewTarget::Tool("mcp_docs_echo".to_owned()))
            .await,
        McpMenuPreview {
            display: format!("{TOOL_HEADING}{schema}"),
            insert: schema.to_owned(),
        }
    );
    let resource = fixture
        .preview(
            2,
            McpMenuPreviewTarget::Resource {
                server: "docs".to_owned(),
                uri: "custom://alpha".to_owned(),
            },
        )
        .await;
    assert_eq!(
        resource.insert,
        "line one\nline two\n\n[binary MCP resource content omitted]"
    );
    assert_eq!(
        resource.display,
        format!("{RESOURCE_HEADING}line one line two [binary MCP resource content omitted]")
    );
    let prompt = fixture
        .preview(
            3,
            McpMenuPreviewTarget::Prompt {
                server: "docs".to_owned(),
                name: "review".to_owned(),
                arguments: Vec::new(),
            },
        )
        .await;
    assert_eq!(
        prompt.insert,
        "user: {\"type\":\"text\",\"text\":\"Review this\"}\n\nassistant: {\"type\":\"text\",\"text\":\"Sure\"}"
    );
    assert!(prompt.display.starts_with(PROMPT_HEADING));
}

#[tokio::test]
async fn a_preview_that_cannot_load_settles_the_menu_with_its_error() {
    let fixture = fixture().await;
    assert_eq!(
        fixture
            .failure(1, McpMenuPreviewTarget::Tool("mcp_docs_absent".to_owned()))
            .await,
        McpMenuSettled {
            generation: 1,
            outcome: McpMenuOutcome::Failed,
            feedback: "MCP preview failed: McpToolNotFound".to_owned(),
        }
    );
    assert_eq!(
        fixture
            .failure(
                2,
                McpMenuPreviewTarget::Resource {
                    server: "absent".to_owned(),
                    uri: "custom://alpha".to_owned(),
                },
            )
            .await
            .feedback,
        "MCP preview failed: McpServerNotFound"
    );
    assert_eq!(
        fixture
            .feedback(3, McpMenuPreviewTarget::Tool("mcp_docs_wide".to_owned()))
            .await,
        "MCP preview failed: McpAccessDenied"
    );
    assert_eq!(
        fixture
            .feedback(
                4,
                McpMenuPreviewTarget::Resource {
                    server: "docs".to_owned(),
                    uri: "custom://broken".to_owned(),
                },
            )
            .await,
        "MCP preview failed: McpProtocolError"
    );
    assert_eq!(
        fixture
            .feedback(
                5,
                McpMenuPreviewTarget::Prompt {
                    server: "docs".to_owned(),
                    name: "tone".to_owned(),
                    arguments: Vec::new(),
                },
            )
            .await,
        "MCP preview failed: InvalidArguments"
    );
}

#[test]
fn a_preview_is_bounded_on_a_character_boundary_and_displayed_on_one_line() {
    let raw = format!("{}\u{1b}]0;owned\u{7}", "é".repeat(40_000));
    let preview = menu_preview(RESOURCE_HEADING, &raw);
    assert_eq!(preview.insert.len(), MAX_PREVIEW_BYTES);
    assert!(raw.starts_with(&preview.insert));
    let flattened = menu_preview(PROMPT_HEADING, "one\n\ttwo\u{1b}]0;owned\u{7}");
    assert_eq!(flattened.insert, "one\n\ttwo\u{1b}]0;owned\u{7}");
    assert_eq!(
        flattened.display,
        format!("{PROMPT_HEADING}one two\\x1b]0;owned\\x07")
    );
}

#[tokio::test]
async fn a_prompt_previews_with_its_filled_arguments() {
    let fixture = fixture().await;
    let argument = |name: &str, value: &str| McpMenuArgument {
        name: name.to_owned(),
        value: value.to_owned(),
    };
    let preview = fixture
        .preview(
            1,
            McpMenuPreviewTarget::Prompt {
                server: "docs".to_owned(),
                name: "tone".to_owned(),
                arguments: vec![argument("tone", "warm"), argument("extra", "")],
            },
        )
        .await;
    assert_eq!(
        preview.insert,
        "user: {\"type\":\"text\",\"text\":\"Warm review\"}"
    );
    assert_eq!(
        arguments_json(&[argument("topic", "a \"b\""), argument("tone", "")]),
        r#"{"topic":"a \"b\""}"#
    );
}
