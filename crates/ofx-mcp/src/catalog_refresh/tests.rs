use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use ofx_config::{ContextLimitName, ContextLimits};
use ofx_contract::DynamicTools;

use super::*;
use crate::catalog_freshness::Freshness;
use crate::health::CacheFreshness;
use crate::mcp_contract::{EnvVar, McpServerConfig};
use crate::mcp_runtime::McpRuntime;
use crate::native_config::NativeConfigLoad;
use crate::server_lifecycle::{Advertised, Server};
use crate::server_transport::ConnectOptions;
use crate::server_views::snapshot_server;
use crate::startup_admission::StartupPhase;
use crate::timing::{sleep, timeout};
use crate::tool_mcp_registry::SchemaLimits;
use crate::tool_operations::CallOptions;
use crate::transport::ShutdownMode;

const SERVER: &str = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{\"listChanged\":true}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"}}" ;;
    *'"method":"tools/list"'*)
      echo list >> "$STATE/lists"
      name=$(cat "$STATE/name" 2>/dev/null || echo alpha)
      ttl=$(cat "$STATE/ttl" 2>/dev/null || echo 60000)
      result="{\"ttlMs\":$ttl,\"tools\":[{\"name\":\"$name\",\"inputSchema\":{\"type\":\"object\"}}]}"
      if [ -f "$STATE/fail" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32603,"message":"unavailable"}}\n' "$id"
      elif [ -f "$STATE/slow" ]; then
        ( sleep 1; reply "$id" "$result" ) &
      else
        reply "$id" "$result"
      fi ;;
    *'"method":"tools/call"'*) reply "$id" '{"content":[{"type":"text","text":"called"}]}' ;;
  esac
done
"#;

fn config(state: &Path) -> McpServerConfig {
    let mut config = McpServerConfig::stdio(
        "fixture",
        "/bin/sh",
        vec!["-c".to_owned(), SERVER.to_owned()],
    );
    config.env.push(EnvVar {
        key: "STATE".to_owned(),
        value: state.to_string_lossy().into_owned(),
    });
    config
}

fn write(state: &Path, name: &str, content: &str) {
    std::fs::write(state.join(name), content).unwrap();
}

fn lists(state: &Path) -> usize {
    std::fs::read_to_string(state.join("lists"))
        .unwrap_or_default()
        .lines()
        .count()
}

async fn started(state: &Path) -> Arc<Server> {
    let server = Arc::new(Server::new(
        config(state),
        ConnectOptions::default(),
        Arc::new(AtomicU64::new(0)),
    ));
    server.start().await;
    server
}

fn advertised(server: &Server) -> Advertised {
    let (catalog, instructions) = server.catalog().unwrap();
    Advertised {
        tool: catalog.tools[0].clone(),
        instructions,
    }
}

async fn call(server: &Arc<Server>, advertised: &Advertised) -> bool {
    server
        .call(advertised, "{}", CallOptions::default())
        .await
        .is_ok()
}

fn health(server: &Server) -> (CacheFreshness, u8, Option<u64>) {
    let snapshot = snapshot_server(server);
    (
        snapshot.cache_freshness,
        snapshot.retry_attempt,
        snapshot.retry_in_ms,
    )
}

fn ready_client(server: &Server) -> Arc<McpClient> {
    match server.lifecycle() {
        crate::server_lifecycle::Lifecycle::Ready(client) => client,
        _ => panic!("the server is not ready"),
    }
}

#[tokio::test]
async fn a_tool_list_expires_after_its_lifetime_and_a_call_lists_it_again() {
    let state = tempfile::tempdir().unwrap();
    write(state.path(), "ttl", "100");
    let server = started(state.path()).await;
    let alpha = advertised(&server);
    assert_eq!(health(&server), (CacheFreshness::Fresh, 0, None));
    assert!(call(&server, &alpha).await);
    assert_eq!(lists(state.path()), 1);
    sleep(Duration::from_millis(150)).await;
    assert_eq!(health(&server).0, CacheFreshness::Stale);
    write(state.path(), "ttl", "60000");
    assert!(call(&server, &alpha).await);
    assert_eq!(lists(state.path()), 2);
    assert_eq!(health(&server), (CacheFreshness::Fresh, 0, None));
    assert!(call(&server, &alpha).await);
    assert_eq!(lists(state.path()), 2);
    server.stop(ShutdownMode::Immediate).await;
}

#[tokio::test]
async fn a_failed_refresh_keeps_the_tools_and_retries_after_its_backoff() {
    let state = tempfile::tempdir().unwrap();
    write(state.path(), "ttl", "0");
    let server = started(state.path()).await;
    let alpha = advertised(&server);
    write(state.path(), "fail", "");
    assert!(call(&server, &alpha).await);
    assert_eq!(lists(state.path()), 2);
    let (freshness, attempt, retry_in_ms) = health(&server);
    assert_eq!((freshness, attempt), (CacheFreshness::FailedRefresh, 1));
    assert!(
        retry_in_ms.is_some_and(|delay| delay <= 100),
        "{retry_in_ms:?}"
    );
    assert!(call(&server, &alpha).await);
    assert_eq!(lists(state.path()), 2);
    sleep(Duration::from_millis(120)).await;
    assert!(call(&server, &alpha).await);
    assert_eq!(lists(state.path()), 3);
    let (freshness, attempt, retry_in_ms) = health(&server);
    assert_eq!((freshness, attempt), (CacheFreshness::FailedRefresh, 2));
    assert!(
        retry_in_ms.is_some_and(|delay| delay <= 200),
        "{retry_in_ms:?}"
    );
    std::fs::remove_file(state.path().join("fail")).unwrap();
    write(state.path(), "ttl", "60000");
    sleep(Duration::from_millis(220)).await;
    assert!(call(&server, &alpha).await);
    assert_eq!(lists(state.path()), 4);
    assert_eq!(health(&server), (CacheFreshness::Fresh, 0, None));
    server.stop(ShutdownMode::Immediate).await;
}

#[tokio::test]
async fn a_refresh_in_flight_shows_refreshing_and_others_keep_the_last_list() {
    let state = tempfile::tempdir().unwrap();
    let server = started(state.path()).await;
    let client = ready_client(&server);
    write(state.path(), "slow", "");
    write(state.path(), "name", "beta");
    client.request_tool_refresh();
    let refreshing = {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            client
                .refresh_tools(Instant::now() + Duration::from_secs(5))
                .await
                .replaced
        })
    };
    while lists(state.path()) < 2 {
        sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(health(&server).0, CacheFreshness::Refreshing);
    let concurrent = client
        .refresh_tools(Instant::now() + Duration::from_secs(5))
        .await;
    assert!(!concurrent.replaced);
    assert_eq!(concurrent.catalog.tools[0].name, "alpha");
    assert!(refreshing.await.unwrap());
    assert_eq!(client.tool_catalog().tools[0].name, "beta");
    assert_eq!(health(&server).0, CacheFreshness::Fresh);
    assert_eq!(lists(state.path()), 2);
    server.stop(ShutdownMode::Immediate).await;
}

#[tokio::test]
async fn a_refresh_that_is_abandoned_counts_as_failed() {
    let state = tempfile::tempdir().unwrap();
    let server = started(state.path()).await;
    let client = ready_client(&server);
    write(state.path(), "slow", "");
    client.request_tool_refresh();
    assert!(
        timeout(
            Duration::from_millis(50),
            client.refresh_tools(Instant::now() + Duration::from_secs(5)),
        )
        .await
        .is_err()
    );
    let metadata = client.tool_snapshot().metadata;
    assert_eq!(metadata.freshness, Freshness::FailedRefresh);
    assert_eq!(metadata.refresh_attempt, 1);
    assert_eq!(client.tool_catalog().tools[0].name, "alpha");
    server.stop(ShutdownMode::Immediate).await;
}

#[tokio::test]
async fn a_list_that_expired_is_refreshed_in_the_background_when_tools_are_gathered() {
    let state = tempfile::tempdir().unwrap();
    write(state.path(), "ttl", "0");
    let limits = ContextLimits::default();
    let runtime = McpRuntime::new(
        NativeConfigLoad {
            configs: vec![config(state.path())],
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        SchemaLimits {
            server_instructions: limits.get(ContextLimitName::McpServerInstructionsBytes),
            selected_schema: limits.get(ContextLimitName::McpSelectedSchemaBytes),
        },
    );
    runtime.connect(StartupPhase::All).await;
    let names = |runtime: &McpRuntime| -> Vec<String> {
        runtime
            .tools()
            .iter()
            .map(|tool| tool.spec().name.clone())
            .collect()
    };
    write(state.path(), "name", "beta");
    write(state.path(), "ttl", "60000");
    let generation = runtime.generation();
    assert_eq!(names(&runtime), ["mcp_fixture_alpha"]);
    for _ in 0..200 {
        if runtime.generation() != generation {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(names(&runtime), ["mcp_fixture_beta"]);
    assert_eq!(lists(state.path()), 2);
    let settled = runtime.generation();
    assert_eq!(names(&runtime), ["mcp_fixture_beta"]);
    sleep(Duration::from_millis(50)).await;
    assert_eq!(runtime.generation(), settled);
    assert_eq!(lists(state.path()), 2);
    runtime.shutdown(ShutdownMode::Immediate).await;
}
