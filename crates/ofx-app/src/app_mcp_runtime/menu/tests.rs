use std::sync::Mutex;
use std::time::Duration;

use ofx_config::ContextLimits;
use ofx_contract::{McpMenuConnection, McpMenuSource, McpMenuStatus, McpMenuTransport};
use ofx_mcp::{
    ConnectOptions, IssuerMismatch, IssuerMismatchSource, McpServerConfig, NativeConfigLoad,
    TransportType,
};

use super::super::{McpSources, lock};
use super::*;

struct Fixture {
    host: McpHost,
    events: Arc<Mutex<Vec<UiEvent>>>,
    _listener: std::net::TcpListener,
    _data: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let data = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(data.path()).unwrap();
    let runtime = Arc::new(McpRuntime::new(
        NativeConfigLoad {
            configs: vec![McpServerConfig::remote("docs", TransportType::Http, &url)],
            ..NativeConfigLoad::default()
        },
        ConnectOptions {
            profile_data: Some(root.clone()),
            ..ConnectOptions::default()
        },
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
    Fixture {
        host,
        events,
        _listener: listener,
        _data: data,
    }
}

async fn settled(events: &Mutex<Vec<UiEvent>>) -> (McpMenuSettled, McpMenuSnapshot) {
    for _ in 0..500 {
        let found = lock(events).iter().find_map(|event| match event {
            UiEvent::McpMenuSettled { settled, snapshot } => {
                Some((settled.clone(), snapshot.clone()))
            }
            _ => None,
        });
        if let Some(found) = found {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the menu request never settled: {:?}", lock(events));
}

#[test]
fn reload_feedback_follows_upstreams_menu_wording() {
    let published = |unavailable: &[&str], healthy| {
        Ok(ReloadOutcome::Published {
            configured: 2,
            unavailable: unavailable.iter().map(|name| (*name).to_owned()).collect(),
            healthy,
        })
    };
    assert_eq!(
        reload_feedback(published(&[], true)),
        Some((true, "MCP configuration reloaded.".to_owned()))
    );
    assert_eq!(
        reload_feedback(published(&["alpha"], false)),
        Some((true, "MCP reloaded with 1 unavailable server.".to_owned()))
    );
    assert_eq!(
        reload_feedback(published(&["alpha", "beta"], false)),
        Some((true, "MCP reloaded with 2 unavailable servers.".to_owned()))
    );
    assert_eq!(
        reload_feedback(Ok(ReloadOutcome::RetainedRequiredFailure(
            "boom".to_owned()
        ))),
        Some((
            false,
            "MCP reload failed; the previous runtime remains active.".to_owned()
        ))
    );
    assert_eq!(
        reload_feedback(Err(ReloadFailure::AuthorityReduced)),
        Some((
            false,
            "MCP reload failed: McpAuthorityReducedReloadFailed".to_owned()
        ))
    );
    assert_eq!(
        reload_feedback(Err(ReloadFailure::Failed("LockBusy".to_owned()))),
        Some((false, "MCP reload failed: LockBusy".to_owned()))
    );
    assert_eq!(reload_feedback(Err(ReloadFailure::Cancelled)), None);
}

#[test]
fn authentication_feedback_follows_upstreams_menu_wording() {
    let authenticated =
        |repaired_entries| Ok(AuthenticationOutcome::Authenticated { repaired_entries });
    assert_eq!(
        authentication_feedback("docs", authenticated(0), None),
        (true, "Authenticated MCP server 'docs'.".to_owned())
    );
    assert_eq!(
        authentication_feedback("docs", authenticated(3), None),
        (
            true,
            "Authenticated 'docs'; repaired 3 credential entries.".to_owned()
        )
    );
    assert_eq!(
        authentication_feedback("docs", authenticated(0), Some(McpError::McpConnectionClosed)),
        (
            false,
            "Authenticated MCP server 'docs'. Connection failed: McpConnectionClosed. Check /mcp list."
                .to_owned()
        )
    );
    let mismatch = IssuerMismatch {
        expected: "https://a.example".to_owned(),
        returned: "https://b.example".to_owned(),
        source: IssuerMismatchSource::AuthorizationMetadata,
    };
    assert_eq!(
        authentication_feedback(
            "docs",
            Ok(AuthenticationOutcome::IssuerMismatch(mismatch)),
            None
        ),
        (
            false,
            "MCP authentication for 'docs' rejected an issuer mismatch.".to_owned()
        )
    );
    assert_eq!(
        authentication_feedback("docs", Err(McpError::McpAuthorizationCallbackTimedOut), None),
        (
            false,
            "MCP authentication for 'docs' failed: Authorization timed out. Run the connection command again and finish authorization in your browser while oh-fx stays open".to_owned()
        )
    );
}

#[tokio::test]
async fn a_refresh_answers_with_the_current_servers() {
    let Fixture { host, events, .. } = fixture();
    host.handle_menu(McpMenuRequest::Refresh);
    let snapshot = match lock(&events).as_slice() {
        [UiEvent::McpMenuRefreshed { snapshot }] => snapshot.clone(),
        other => panic!("expected one refresh, got {other:?}"),
    };
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

#[tokio::test]
async fn a_menu_reload_settles_its_own_generation_with_the_new_servers() {
    let Fixture { host, events, .. } = fixture();
    host.handle_menu(McpMenuRequest::Reload { generation: 7 });
    let (settled, snapshot) = settled(&events).await;
    assert_eq!(
        settled,
        McpMenuSettled {
            generation: 7,
            succeeded: true,
            feedback: "MCP configuration reloaded.".to_owned(),
        }
    );
    assert!(snapshot.servers.is_empty());
    assert!(
        !lock(&events)
            .iter()
            .any(|event| matches!(event, UiEvent::Notice { .. }))
    );
}

#[tokio::test]
async fn a_refused_menu_sign_in_settles_as_a_failed_operation() {
    let Fixture { host, events, .. } = fixture();
    host.handle_menu(McpMenuRequest::Authenticate {
        generation: 3,
        server: "missing".to_owned(),
    });
    let (refused, _) = settled(&events).await;
    assert_eq!(
        refused,
        McpMenuSettled {
            generation: 3,
            succeeded: false,
            feedback: "MCP operation failed: McpServerNotFound".to_owned(),
        }
    );
    lock(&events).clear();
    assert_eq!(
        host.begin_authentication("docs"),
        crate::mcp_auth_notices::waiting("docs")
    );
    host.handle_menu(McpMenuRequest::Authenticate {
        generation: 4,
        server: "docs".to_owned(),
    });
    let (busy, _) = settled(&events).await;
    assert_eq!(
        busy,
        McpMenuSettled {
            generation: 4,
            succeeded: false,
            feedback: "MCP operation failed: McpAuthenticationBusy".to_owned(),
        }
    );
    host.discard_authentication("test");
}
