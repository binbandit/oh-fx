use std::sync::Mutex;
use std::time::Duration;

use ofx_config::ContextLimits;
use ofx_contract::McpMenuOutcome;
use ofx_mcp::{ConnectOptions, McpServerConfig, NativeConfigLoad, StartupPhase};

use super::super::super::{McpSources, lock};
use super::*;

const COMPLETING: &str = r#"reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{},\"resources\":{},\"prompts\":{},\"completions\":{}},\"serverInfo\":{\"name\":\"docs\",\"version\":\"1\"}}" ;;
    *'"method":"tools/list"'*) reply "$id" '{"tools":[]}' ;;
    *'"method":"prompts/list"'*) reply "$id" '{"prompts":[{"name":"review","arguments":[{"name":"topic","required":true},{"name":"tone"}]}]}' ;;
    *'"method":"resources/list"'*) reply "$id" '{"resources":[]}' ;;
    *'"method":"resources/templates/list"'*) reply "$id" '{"resourceTemplates":[{"uriTemplate":"custom://project/{path}","name":"project"}]}' ;;
    *'"method":"completion/complete"'*'"name":"tone"'*) reply "$id" '{"completion":{"values":[]}}' ;;
    *'"method":"completion/complete"'*'"topic":"alpha"'*) reply "$id" '{"completion":{"values":["friendly","formal"]}}' ;;
    *'"method":"completion/complete"'*'ref/resource'*) reply "$id" '{"completion":{"values":["src/lib"]}}' ;;
    *'"method":"completion/complete"'*) reply "$id" '{"completion":{"values":["alpha","beta"]}}' ;;
  esac
done"#;

struct Fixture {
    host: McpHost,
    events: Arc<Mutex<Vec<UiEvent>>>,
    _home: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let runtime = Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs: vec![McpServerConfig::stdio(
                "docs",
                "/bin/sh",
                vec!["-c".to_owned(), COMPLETING.to_owned()],
            )],
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

fn argument(name: &str, value: &str) -> McpMenuArgument {
    McpMenuArgument {
        name: name.to_owned(),
        value: value.to_owned(),
    }
}

fn prompt() -> McpMenuCompletionTarget {
    McpMenuCompletionTarget::Prompt {
        server: "docs".to_owned(),
        name: "review".to_owned(),
    }
}

impl Fixture {
    async fn complete(
        &self,
        generation: u64,
        target: McpMenuCompletionTarget,
        current: McpMenuArgument,
        context: Vec<McpMenuArgument>,
    ) -> Result<String, String> {
        self.host
            .complete_menu_argument(generation, target, current, context);
        for _ in 0..1000 {
            let found = lock(&self.events).iter().find_map(|event| match event {
                UiEvent::McpMenuCompleted {
                    generation: g,
                    value,
                } if *g == generation => Some(Ok(value.clone())),
                UiEvent::McpMenuSettled { settled, .. } if settled.generation == generation => {
                    assert_eq!(settled.outcome, McpMenuOutcome::Failed);
                    Some(Err(settled.feedback.clone()))
                }
                _ => None,
            });
            if let Some(result) = found {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no completion for {generation}: {:?}", lock(&self.events));
    }
}

#[tokio::test]
async fn a_completion_takes_the_servers_first_value_with_the_other_fields_as_context() {
    let fixture = fixture().await;
    assert_eq!(
        fixture
            .complete(
                1,
                prompt(),
                argument("topic", "al"),
                vec![argument("tone", "")]
            )
            .await,
        Ok("alpha".to_owned())
    );
    assert_eq!(
        fixture
            .complete(
                2,
                prompt(),
                argument("tone", ""),
                vec![argument("topic", "alpha")]
            )
            .await,
        Err("MCP completion failed: McpCompletionEmpty".to_owned())
    );
    assert_eq!(
        fixture
            .complete(
                3,
                McpMenuCompletionTarget::Resource {
                    server: "docs".to_owned(),
                    uri_template: "custom://project/{path}".to_owned(),
                },
                argument("path", "src"),
                Vec::new(),
            )
            .await,
        Ok("src/lib".to_owned())
    );
    assert_eq!(
        fixture
            .complete(
                4,
                McpMenuCompletionTarget::Prompt {
                    server: "absent".to_owned(),
                    name: "review".to_owned(),
                },
                argument("topic", ""),
                Vec::new(),
            )
            .await,
        Err("MCP completion failed: McpServerNotFound".to_owned())
    );
}
