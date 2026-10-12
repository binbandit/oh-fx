use std::sync::{Arc, Mutex};

use ofx_config::ContextLimits;
use ofx_contract::{McpMenuConnection, McpMenuSource, McpMenuStatus, McpMenuTransport};
use ofx_mcp::{ConnectOptions, McpRuntime, McpServerConfig, NativeConfigLoad, TransportType};

use super::super::{McpSources, lock};
use super::*;

#[tokio::test]
async fn a_refresh_answers_with_the_current_servers() {
    let data = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(data.path()).unwrap();
    let runtime = Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs: vec![McpServerConfig::remote(
                "docs",
                TransportType::Http,
                "http://127.0.0.1:9/mcp",
            )],
            ..NativeConfigLoad::default()
        },
        ConnectOptions::default(),
        Vec::new(),
        ContextLimits::default(),
    ));
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&events);
    let host = McpHost::new(
        runtime,
        McpSources::new(None, root),
        Arc::new(move |event| lock(&seen).push(event)),
    );
    host.handle_menu(McpMenuRequest::Refresh);
    let snapshot = match lock(&events).as_slice() {
        [UiEvent::McpMenuRefreshed { snapshot }] => snapshot.clone(),
        other => panic!("expected one refresh, got {other:?}"),
    };
    assert_eq!(snapshot, host.menu_snapshot());
    assert_eq!(snapshot.profile_config, None);
    assert_eq!(snapshot.servers.len(), 1);
    let docs = &snapshot.servers[0];
    assert_eq!(docs.name, "docs");
    assert_eq!(docs.source, McpMenuSource::Profile);
    assert_eq!(docs.transport, McpMenuTransport::Http);
    assert_eq!(docs.connection, McpMenuConnection::Disconnected);
    assert_eq!(docs.status, McpMenuStatus::Unavailable);
    assert!(!docs.reloading);
}
