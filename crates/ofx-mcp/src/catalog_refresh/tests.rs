use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use super::*;
use crate::catalog_freshness::Freshness;
use crate::error::McpError;
use crate::health::CacheFreshness;
use crate::mcp_contract::{EnvVar, McpServerConfig};
use crate::server_lifecycle::{Advertised, CallFailure, Server};
use crate::server_transport::ConnectOptions;
use crate::server_views::snapshot_server;
use crate::timing::{sleep, timeout};
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
      name=$(cat "$STATE/name" 2>/dev/null || echo alpha)
      ttl=$(cat "$STATE/ttl" 2>/dev/null || echo 60000)
      result="{\"ttlMs\":$ttl,\"tools\":[{\"name\":\"$name\",\"inputSchema\":{\"type\":\"object\"}}]}"
      echo list >> "$STATE/lists"
      if [ -f "$STATE/fail" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32603,"message":"unavailable"}}\n' "$id"
      elif [ -f "$STATE/slow" ]; then
        held=$(cat "$STATE/slow")
        (
          if [ "$held" = held ]; then
            while [ ! -f "$STATE/release" ]; do sleep 0.01; done
          else
            sleep 1
          fi
          reply "$id" "$result"
        ) &
      else
        reply "$id" "$result"
      fi ;;
    *'"method":"tools/call"'*)
      echo call >> "$STATE/calls"
      reply "$id" '{"content":[{"type":"text","text":"called"}]}'
      if [ -f "$STATE/notify" ]; then
        rm "$STATE/notify"
        printf '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}\n'
      fi ;;
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

fn lines(state: &Path, name: &str) -> usize {
    std::fs::read_to_string(state.join(name))
        .unwrap_or_default()
        .lines()
        .count()
}

fn lists(state: &Path) -> usize {
    lines(state, "lists")
}

fn hold_listings(state: &Path) {
    write(state, "slow", "held");
}

fn release_listings(state: &Path) {
    write(state, "release", "");
}

async fn until_notified(client: &McpClient) {
    let notified = Instant::now() + Duration::from_secs(5);
    while !client.tools_invalidation.pending() && Instant::now() < notified {
        sleep(Duration::from_millis(5)).await;
    }
    assert!(client.tools_invalidation.pending());
}

async fn started(state: &Path) -> Arc<Server> {
    started_with(config(state)).await
}

async fn started_with(config: McpServerConfig) -> Arc<Server> {
    let server = Arc::new(Server::new(
        config,
        ConnectOptions::default(),
        Arc::new(AtomicU64::new(0)),
    ));
    let _ = server.start().await;
    server
}

async fn started_with_operation_timeout(state: &Path, operation_timeout_ms: u32) -> Arc<Server> {
    let mut config = config(state);
    config.operation_timeout_ms = operation_timeout_ms;
    started_with(config).await
}

async fn call_directly(client: &McpClient, advertised: &Advertised) -> bool {
    client
        .call_tool(
            &advertised.tool.name,
            "{}",
            CallOptions::default(),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .is_ok()
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
    let failed = ready_client(&server).tool_snapshot().metadata;
    assert_eq!(
        decide_refresh(failed, failed.retry_at_ms - 1, false),
        RefreshAction::RetryLater
    );
    assert_eq!(
        decide_refresh(failed, failed.retry_at_ms, false),
        RefreshAction::Refresh
    );
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
    assert!(concurrent.in_flight);
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
async fn a_call_waits_for_the_list_a_change_notification_is_fetching() {
    let state = tempfile::tempdir().unwrap();
    let server = started(state.path()).await;
    let alpha = advertised(&server);
    write(state.path(), "slow", "");
    write(state.path(), "notify", "");
    assert!(call(&server, &alpha).await);
    while lists(state.path()) < 2 {
        sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(health(&server).0, CacheFreshness::Refreshing);
    assert!(call(&server, &alpha).await);
    assert_eq!(lists(state.path()), 2);
    assert!(!ready_client(&server).tools_invalidation.pending());
    assert_eq!(health(&server).0, CacheFreshness::Fresh);
    server.stop(ShutdownMode::Immediate).await;
}

#[tokio::test]
async fn a_call_is_refused_while_a_change_notification_waits_for_its_list() {
    let state = tempfile::tempdir().unwrap();
    let server = started(state.path()).await;
    let alpha = advertised(&server);
    write(state.path(), "fail", "");
    write(state.path(), "notify", "");
    assert!(call(&server, &alpha).await);
    while lists(state.path()) < 2 {
        sleep(Duration::from_millis(5)).await;
    }
    let refused = server.call(&alpha, "{}", CallOptions::default()).await;
    assert!(matches!(
        refused,
        Err(CallFailure::Mcp(McpError::McpToolCatalogChanged))
    ));
    assert_eq!(lists(state.path()), 2);
    std::fs::remove_file(state.path().join("fail")).unwrap();
    sleep(Duration::from_millis(120)).await;
    assert!(call(&server, &alpha).await);
    assert_eq!(lists(state.path()), 3);
    assert_eq!(health(&server), (CacheFreshness::Fresh, 0, None));
    server.stop(ShutdownMode::Immediate).await;
}

#[tokio::test]
async fn a_call_goes_ahead_with_the_last_list_while_a_list_with_no_change_pending_is_in_flight() {
    let state = tempfile::tempdir().unwrap();
    let server = started(state.path()).await;
    let alpha = advertised(&server);
    let client = ready_client(&server);
    hold_listings(state.path());
    client.request_tool_refresh();
    let refreshing = {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            client
                .refresh_tools(Instant::now() + Duration::from_secs(5))
                .await
        })
    };
    while lists(state.path()) < 2 {
        sleep(Duration::from_millis(5)).await;
    }
    assert!(call(&server, &alpha).await);
    assert_eq!(lines(state.path(), "calls"), 1);
    assert_eq!(health(&server).0, CacheFreshness::Refreshing);
    release_listings(state.path());
    refreshing.await.unwrap();
    assert_eq!(health(&server).0, CacheFreshness::Fresh);
    server.stop(ShutdownMode::Immediate).await;
}

#[tokio::test]
async fn a_call_whose_deadline_passes_while_a_change_is_being_listed_never_reaches_the_server() {
    let state = tempfile::tempdir().unwrap();
    let server = started_with_operation_timeout(state.path(), 50).await;
    let alpha = advertised(&server);
    let client = ready_client(&server);
    hold_listings(state.path());
    client.request_tool_refresh();
    let refreshing = {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            client
                .refresh_tools(Instant::now() + Duration::from_secs(5))
                .await
        })
    };
    while lists(state.path()) < 2 {
        sleep(Duration::from_millis(5)).await;
    }
    write(state.path(), "notify", "");
    assert!(call_directly(&client, &alpha).await);
    until_notified(&client).await;
    let timed_out = server.call(&alpha, "{}", CallOptions::default()).await;
    assert!(matches!(
        timed_out,
        Err(CallFailure::Mcp(McpError::McpRequestTimedOut))
    ));
    release_listings(state.path());
    refreshing.await.unwrap();
    assert!(call_directly(&client, &alpha).await);
    assert_eq!(lines(state.path(), "calls"), 2);
    server.stop(ShutdownMode::Immediate).await;
}

#[tokio::test]
async fn a_call_whose_own_list_uses_up_its_deadline_never_reaches_the_server() {
    let state = tempfile::tempdir().unwrap();
    write(state.path(), "ttl", "0");
    let server = started_with_operation_timeout(state.path(), 50).await;
    let alpha = advertised(&server);
    let client = ready_client(&server);
    write(state.path(), "slow", "");
    let timed_out = server.call(&alpha, "{}", CallOptions::default()).await;
    assert!(matches!(
        timed_out,
        Err(CallFailure::Mcp(McpError::McpRequestTimedOut))
    ));
    assert!(call_directly(&client, &alpha).await);
    assert_eq!(lists(state.path()), 2);
    assert_eq!(lines(state.path(), "calls"), 1);
    server.stop(ShutdownMode::Immediate).await;
}

#[tokio::test]
async fn a_change_notified_while_a_list_is_in_flight_is_listed_once_that_list_ends() {
    let state = tempfile::tempdir().unwrap();
    let server = started(state.path()).await;
    let alpha = advertised(&server);
    let client = ready_client(&server);
    hold_listings(state.path());
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
    write(state.path(), "name", "beta");
    write(state.path(), "notify", "");
    assert!(call_directly(&client, &alpha).await);
    until_notified(&client).await;
    release_listings(state.path());
    assert!(!refreshing.await.unwrap());
    assert!(listed_beta(&client).await);
    assert_eq!(lists(state.path()), 3);
    assert_eq!(health(&server).0, CacheFreshness::Fresh);
    server.stop(ShutdownMode::Immediate).await;
}

async fn notified_twice_while_the_watcher_lists(
    state: &Path,
) -> (Arc<Server>, Advertised, Arc<McpClient>) {
    let server = started(state).await;
    let alpha = advertised(&server);
    let client = ready_client(&server);
    hold_listings(state);
    write(state, "notify", "");
    assert!(call_directly(&client, &alpha).await);
    while lists(state) < 2 {
        sleep(Duration::from_millis(5)).await;
    }
    write(state, "name", "beta");
    write(state, "notify", "");
    assert!(call_directly(&client, &alpha).await);
    sleep(Duration::from_millis(100)).await;
    (server, alpha, client)
}

async fn listed_beta(client: &McpClient) -> bool {
    let listed = Instant::now() + Duration::from_secs(5);
    while client.tool_catalog().tools[0].name != "beta" && Instant::now() < listed {
        sleep(Duration::from_millis(10)).await;
    }
    client.tool_catalog().tools[0].name == "beta" && !client.tools_invalidation.pending()
}

#[tokio::test]
async fn a_change_a_reload_receives_during_the_watchers_list_is_still_listed() {
    let state = tempfile::tempdir().unwrap();
    let (server, _, client) = notified_twice_while_the_watcher_lists(state.path()).await;
    server
        .refresh_tools(&client, Instant::now() + Duration::from_secs(5))
        .await;
    release_listings(state.path());
    assert!(listed_beta(&client).await);
    assert_eq!(lists(state.path()), 3);
    server.stop(ShutdownMode::Immediate).await;
}

#[tokio::test]
async fn a_change_a_cancelled_call_receives_during_the_watchers_list_is_still_listed() {
    let state = tempfile::tempdir().unwrap();
    let (server, alpha, client) = notified_twice_while_the_watcher_lists(state.path()).await;
    assert!(
        timeout(
            Duration::from_millis(50),
            server.call(&alpha, "{}", CallOptions::default())
        )
        .await
        .is_err()
    );
    release_listings(state.path());
    assert!(listed_beta(&client).await);
    assert_eq!(lists(state.path()), 3);
    assert_eq!(lines(state.path(), "calls"), 2);
    server.stop(ShutdownMode::Immediate).await;
}
