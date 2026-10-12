use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ofx_config::ContextLimits;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::super::{McpRuntime, ReloadCancelled, ReloadOutcome};
use crate::mcp_contract::{McpServerConfig, TransportType};
use crate::native_config::NativeConfigLoad;
use crate::server_transport::ConnectOptions;
use crate::startup_admission::StartupPhase;
use crate::test_support::{FakeServer, RecordedRequest, Reply};

fn reply(request: &RecordedRequest) -> Reply {
    let Some(id) = request.request_id() else {
        return Reply::status(202);
    };
    match request.method_name().as_deref() {
        Some("initialize") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-11-25","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"remote","version":"1"}}}}}}"#
        )),
        Some("tools/list") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"echo","inputSchema":{{"type":"object"}}}},{{"name":"read file","inputSchema":{{"type":"object"}}}}]}}}}"#
        )),
        _ => Reply::status(202),
    }
}

struct Held {
    server: FakeServer,
    release: Arc<Notify>,
}

async fn holding_the_second(method: &'static str) -> Held {
    let release = Arc::new(Notify::new());
    let held = Arc::clone(&release);
    let seen = AtomicUsize::new(0);
    let server = FakeServer::start(move |request| {
        let answer = reply(request);
        if request.method_name().as_deref() == Some(method)
            && seen.fetch_add(1, Ordering::AcqRel) == 1
        {
            return answer.released_by(&held);
        }
        answer
    })
    .await;
    Held { server, release }
}

fn config(url: &str, operation_timeout_ms: u32) -> McpServerConfig {
    McpServerConfig {
        operation_timeout_ms,
        startup_timeout_ms: 5_000,
        ..McpServerConfig::remote("remote", TransportType::Http, url)
    }
}

async fn connected(url: &str) -> Arc<McpRuntime> {
    let runtime = Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs: vec![config(url, 30_000)],
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        ContextLimits::default(),
    ));
    runtime.connect(StartupPhase::All).await;
    runtime
}

fn reconciling(
    runtime: &Arc<McpRuntime>,
    config: McpServerConfig,
    cancel: &CancellationToken,
) -> tokio::task::JoinHandle<Result<ReloadOutcome, ReloadCancelled>> {
    let runtime = Arc::clone(runtime);
    let cancel = cancel.clone();
    tokio::spawn(async move {
        runtime
            .reconcile(
                NativeConfigLoad {
                    configs: vec![config],
                    ..NativeConfigLoad::default()
                },
                true,
                true,
                &cancel,
            )
            .await
    })
}

async fn second(server: &FakeServer, method: &str) {
    for _ in 0..500 {
        let seen = server
            .requests()
            .iter()
            .filter(|request| request.method_name().as_deref() == Some(method))
            .count();
        if seen >= 2 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("the second {method} never arrived");
}

fn reloading(runtime: &McpRuntime) -> Vec<bool> {
    runtime
        .menu_snapshot()
        .servers
        .iter()
        .map(|server| server.reloading)
        .collect()
}

#[tokio::test]
async fn a_reload_marks_the_servers_it_replaces_until_it_publishes() {
    let held = holding_the_second("initialize").await;
    let runtime = connected(&held.server.url).await;
    assert_eq!(reloading(&runtime), [false]);
    let cancel = CancellationToken::new();
    let reload = reconciling(&runtime, config(&held.server.url, 7_000), &cancel);
    second(&held.server, "initialize").await;
    assert_eq!(reloading(&runtime), [true]);
    held.release.notify_one();
    assert!(matches!(
        reload.await.unwrap(),
        Ok(ReloadOutcome::Published { .. })
    ));
    assert_eq!(reloading(&runtime), [false]);
    assert_eq!(runtime.current()[0].config.operation_timeout_ms, 7_000);
}

#[tokio::test]
async fn a_cancelled_reload_clears_its_marks() {
    let held = holding_the_second("initialize").await;
    let runtime = connected(&held.server.url).await;
    let original = Arc::clone(&runtime.current()[0]);
    let cancel = CancellationToken::new();
    let reload = reconciling(&runtime, config(&held.server.url, 7_000), &cancel);
    second(&held.server, "initialize").await;
    assert!(original.reload_pending());
    cancel.cancel();
    assert_eq!(reload.await.unwrap(), Err(ReloadCancelled));
    assert!(!original.reload_pending());
    assert!(Arc::ptr_eq(&original, &runtime.current()[0]));
    assert_eq!(reloading(&runtime), [false]);
}

#[tokio::test]
async fn a_reload_marks_kept_servers_while_their_catalogs_refresh() {
    let held = holding_the_second("tools/list").await;
    let runtime = connected(&held.server.url).await;
    let kept = Arc::clone(&runtime.current()[0]);
    let cancel = CancellationToken::new();
    let reload = reconciling(&runtime, config(&held.server.url, 30_000), &cancel);
    second(&held.server, "tools/list").await;
    assert_eq!(reloading(&runtime), [true]);
    held.release.notify_one();
    assert!(matches!(
        reload.await.unwrap(),
        Ok(ReloadOutcome::Published { .. })
    ));
    assert!(Arc::ptr_eq(&kept, &runtime.current()[0]));
    assert_eq!(reloading(&runtime), [false]);
}

#[tokio::test]
async fn the_menu_lists_the_prefixed_tools_of_ready_servers() {
    let held = holding_the_second("initialize").await;
    let runtime = connected(&held.server.url).await;
    assert_eq!(
        runtime.menu_tool_names(),
        ["mcp_remote_echo", "mcp_remote_read_file"]
    );
    let idle = McpRuntime::new(
        NativeConfigLoad::default(),
        ConnectOptions::default(),
        Vec::new(),
        ContextLimits::default(),
    );
    assert!(idle.menu_tool_names().is_empty());
}
