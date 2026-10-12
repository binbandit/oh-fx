use std::fs;
use std::sync::Mutex;
use std::time::Duration;

use ofx_config::{ContextLimits, ProfilePaths};
use ofx_contract::{McpMenuSettled, UiEvent};
use ofx_mcp::{ConnectOptions, McpRuntime, McpServerConfig, NativeConfigLoad};
use serde_json::{Value, json};

use super::super::super::{McpSources, lock};
use super::*;

const SERVER: &str = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"%s","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}\n' "$id" "$version" ;;
    *'"method":"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[]}}\n' "$id" ;;
  esac
done"#;

struct Fixture {
    host: McpHost,
    events: Arc<Mutex<Vec<UiEvent>>>,
    paths: ProfilePaths,
    workspace: std::path::PathBuf,
    _home: tempfile::TempDir,
}

fn fixture(with_home: bool, configs: Vec<McpServerConfig>) -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(home.path()).unwrap();
    let paths = ProfilePaths {
        config: root.join("config"),
        data: root.join("data"),
        state: root.join("state"),
        cache: root.join("cache"),
    };
    fs::create_dir_all(&paths.config).unwrap();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let runtime = Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs,
            ..NativeConfigLoad::default()
        },
        ConnectOptions {
            profile_data: Some(paths.data.clone()),
            ..ConnectOptions::default()
        },
        Vec::new(),
        ContextLimits::default(),
    ));
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&events);
    let host = McpHost::new(
        runtime,
        McpSources::new(with_home.then(|| paths.clone()), workspace.clone()),
        Arc::new(move |event| lock(&seen).push(event)),
    );
    Fixture {
        host,
        events,
        paths,
        workspace,
        _home: home,
    }
}

impl Fixture {
    async fn settled(&self, count: usize) -> Vec<McpMenuSettled> {
        for _ in 0..1000 {
            let settled: Vec<McpMenuSettled> = lock(&self.events)
                .iter()
                .filter_map(|event| match event {
                    UiEvent::McpMenuSettled { settled, .. } => Some(settled.clone()),
                    _ => None,
                })
                .collect();
            if settled.len() >= count {
                return settled;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {count} settlements: {:?}", lock(&self.events));
    }

    fn profile(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.paths.config.join("mcp.json")).unwrap())
            .unwrap()
    }
}

fn settled(generation: u64, outcome: McpMenuOutcome, feedback: &str) -> McpMenuSettled {
    McpMenuSettled {
        generation,
        outcome,
        feedback: feedback.to_owned(),
    }
}

fn local_form(name: &str, target: &str, arguments: &str) -> McpMenuAddForm {
    McpMenuAddForm {
        transport: McpMenuAddTransport::Local,
        name: name.to_owned(),
        target: target.to_owned(),
        arguments: arguments.to_owned(),
    }
}

#[tokio::test]
async fn an_added_server_is_saved_and_the_reload_settles_its_generation() {
    let fixture = fixture(true, Vec::new());
    fixture
        .host
        .add_from_menu(3, &local_form("docs", "/bin/sh", " -c\t'exit 0' "));
    let settlements = fixture.settled(2).await;
    assert_eq!(
        settlements,
        [
            settled(3, McpMenuOutcome::Continuing, SAVED),
            settled(
                3,
                McpMenuOutcome::Succeeded,
                "MCP reloaded with 1 unavailable server."
            ),
        ]
    );
    assert_eq!(
        fixture.profile()["mcp"]["docs"]["command"],
        json!(["/bin/sh", "-c", "'exit", "0'"])
    );
    let http = McpMenuAddForm {
        transport: McpMenuAddTransport::Http,
        name: "remote".to_owned(),
        target: "https://mcp.example/mcp".to_owned(),
        arguments: "ignored".to_owned(),
    };
    fixture.host.add_from_menu(4, &http);
    fixture.settled(4).await;
    assert_eq!(
        fixture.profile()["mcp"]["remote"]["url"],
        "https://mcp.example/mcp"
    );
}

#[tokio::test]
async fn adds_and_removals_that_cannot_run_settle_as_failed_operations() {
    let fixture = fixture(true, Vec::new());
    fixture.host.add_from_menu(1, &local_form("docs", "", ""));
    fixture
        .host
        .add_from_menu(2, &local_form("bad name", "/bin/true", ""));
    fixture.host.remove_from_menu(3, "absent");
    let homeless = fixture_without_home();
    homeless
        .host
        .add_from_menu(4, &local_form("docs", "/bin/true", ""));
    homeless.host.remove_from_menu(5, "docs");
    homeless.host.trust_from_menu(6, McpMenuTrust::ApproveAll);
    assert_eq!(
        fixture.settled(3).await,
        [
            settled(
                1,
                McpMenuOutcome::Failed,
                "MCP operation failed: McpInvalidServerConfig"
            ),
            settled(
                2,
                McpMenuOutcome::Failed,
                "MCP operation failed: McpInvalidServerName"
            ),
            settled(
                3,
                McpMenuOutcome::Failed,
                "MCP operation failed: McpProfileServerNotFound"
            ),
        ]
    );
    assert_eq!(
        homeless.settled(3).await,
        [4, 5, 6].map(|generation| settled(
            generation,
            McpMenuOutcome::Failed,
            "MCP operation failed: HomeNotSet"
        ))
    );
}

fn fixture_without_home() -> Fixture {
    fixture(false, Vec::new())
}

#[tokio::test]
async fn a_removed_profile_server_is_deleted_and_reloaded() {
    let fixture = fixture(true, Vec::new());
    fixture
        .host
        .add_from_menu(1, &local_form("docs", "/bin/true", ""));
    fixture.settled(2).await;
    fixture.host.remove_from_menu(2, "docs");
    let settlements = fixture.settled(4).await;
    assert_eq!(
        settlements[2..],
        [
            settled(2, McpMenuOutcome::Continuing, REMOVED),
            settled(2, McpMenuOutcome::Succeeded, "MCP configuration reloaded."),
        ]
    );
    assert_eq!(fixture.profile()["mcp"], json!({}));
}

#[tokio::test]
async fn trust_changes_save_the_choice_and_reconnect_only_when_needed() {
    let fixture = fixture(true, Vec::new());
    fs::write(
        fixture.workspace.join(".mcp.json"),
        json!({"mcpServers": {"docs": {"command": "/bin/sh", "args": ["-c", SERVER]}}}).to_string(),
    )
    .unwrap();
    fixture
        .host
        .trust_from_menu(1, McpMenuTrust::Approve("docs".to_owned()));
    let approved = fixture.settled(2).await;
    assert_eq!(
        approved[0],
        settled(1, McpMenuOutcome::Continuing, TRUST_UPDATED)
    );
    assert_eq!(approved[1].generation, 1);
    assert_eq!(approved[1].outcome, McpMenuOutcome::Succeeded);
    fixture
        .host
        .trust_from_menu(2, McpMenuTrust::Approve("docs".to_owned()));
    assert_eq!(
        fixture.settled(3).await[2],
        settled(2, McpMenuOutcome::Returned, TRUST_UPDATED)
    );
    fixture
        .host
        .trust_from_menu(3, McpMenuTrust::Reject("docs".to_owned()));
    let rejected = fixture.settled(5).await;
    assert_eq!(
        rejected[3],
        settled(3, McpMenuOutcome::Continuing, TRUST_UPDATED)
    );
    assert_eq!(rejected[4].generation, 3);
    let settings: Value = serde_json::from_str(
        &fs::read_to_string(fixture.paths.config.join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        settings["workspaces"][fixture.workspace.to_string_lossy().as_ref()]["disabledMcpjsonServers"],
        json!(["docs"])
    );
}

#[tokio::test]
async fn a_menu_logout_returns_to_the_servers_with_upstreams_wording() {
    let fixture = fixture(
        true,
        vec![McpServerConfig::remote(
            "remote",
            ofx_mcp::TransportType::Http,
            "http://127.0.0.1:9/mcp",
        )],
    );
    fixture.host.begin_menu_logout(1, "remote".to_owned());
    fixture.host.begin_menu_logout(2, "absent".to_owned());
    let mut settlements = fixture.settled(2).await;
    settlements.sort_by_key(|settled| settled.generation);
    assert_eq!(
        settlements,
        [
            settled(
                1,
                McpMenuOutcome::Returned,
                "No stored MCP credentials found for 'remote'."
            ),
            settled(
                2,
                McpMenuOutcome::Failed,
                "MCP action failed: McpServerNotFound"
            ),
        ]
    );
}

#[test]
fn logout_feedback_follows_upstreams_menu_wording() {
    let result = |removed, revocation_failed| {
        Ok(LogoutResult {
            removed,
            revocation_failed,
            repaired_entries: 2,
            local_only: false,
        })
    };
    assert_eq!(
        logout_feedback("docs", result(true, true)),
        (
            McpMenuOutcome::Returned,
            "Logged out of 'docs' locally; remote revocation failed.".to_owned()
        )
    );
    assert_eq!(
        logout_feedback("docs", result(true, false)),
        (
            McpMenuOutcome::Returned,
            "Logged out of MCP server 'docs'. Run /mcp auth docs --open to sign in again."
                .to_owned()
        )
    );
}
