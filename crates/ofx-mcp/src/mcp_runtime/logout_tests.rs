use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ofx_config::ContextLimits;
use ofx_contract::{DynamicTools, ToolResultStatus};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::tests::call;
use super::{McpRuntime, ReloadOutcome};
use crate::error::McpError;
use crate::mcp_auth::Credentials;
use crate::mcp_auth_store::{CredentialStore, GrantLookup};
use crate::mcp_contract::{ConfigSource, McpServerConfig, TransportType, WorkspaceAdmission};
use crate::model_catalog::Availability;
use crate::native_config::NativeConfigLoad;
use crate::server_auth::LogoutResult;
use crate::server_lifecycle::Lifecycle;
use crate::server_transport::ConnectOptions;
use crate::startup_admission::StartupPhase;
use crate::test_support::{FakeServer, RecordedRequest, Reply};

const SECRETS: [&str; 2] = ["stored-access", "stored-refresh"];

fn reply(request: &RecordedRequest, revocation_status: u16) -> Reply {
    if request.path == "/revoke" {
        return Reply::status(revocation_status);
    }
    if request.method == "DELETE" {
        return Reply::status(200);
    }
    let Some(id) = request.request_id() else {
        return Reply::status(202);
    };
    match request.method_name().as_deref() {
        Some("initialize") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-11-25","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"remote","version":"1"}}}}}}"#
        ))
        .header("Mcp-Session-Id", "session-1"),
        Some("tools/list") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"echo","inputSchema":{{"type":"object"}}}}]}}}}"#
        )),
        Some("tools/call") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[]}}}}"#
        )),
        _ => Reply::status(202),
    }
}

struct Fixture {
    server: FakeServer,
    _data: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    async fn start(revocation_status: u16) -> Self {
        Self::serving(FakeServer::start(move |request| reply(request, revocation_status)).await)
    }

    fn serving(server: FakeServer) -> Self {
        let data = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(data.path()).unwrap().join("oh-fx");
        Self {
            server,
            _data: data,
            root,
        }
    }

    fn config(&self) -> McpServerConfig {
        McpServerConfig {
            allow_stored_credentials: true,
            startup_timeout_ms: 5_000,
            ..McpServerConfig::remote("remote", TransportType::Http, &self.server.url)
        }
    }

    fn store(&self) -> CredentialStore {
        CredentialStore::new(&self.root)
    }

    fn file(&self) -> PathBuf {
        self.root.join("mcp-credentials/credentials.json")
    }

    fn grant(&self, revocation: bool) -> Credentials {
        Credentials {
            endpoint: self.server.url.clone(),
            resource: self.server.url.clone(),
            issuer: "https://issuer.example".to_owned(),
            client_id: "client".to_owned(),
            client_secret: None,
            access_token: Zeroizing::new("stored-access".to_owned()),
            refresh_token: Some(Zeroizing::new("stored-refresh".to_owned())),
            scope: "tools".to_owned(),
            token_type: "Bearer".to_owned(),
            token_endpoint_auth_method: "none".to_owned(),
            expires_at_ms: i64::MAX,
            authorization_endpoint: "https://issuer.example/authorize".to_owned(),
            token_endpoint: self.server.url.replace("/mcp", "/token"),
            revocation_endpoint: revocation.then(|| self.server.url.replace("/mcp", "/revoke")),
        }
    }

    fn save(&self, config: &McpServerConfig, revocation: bool) {
        self.store()
            .save(
                &GrantLookup::for_server(config).unwrap(),
                &self.grant(revocation),
            )
            .unwrap();
    }

    fn runtime(&self, configs: Vec<McpServerConfig>, profile: bool) -> McpRuntime {
        McpRuntime::new(
            NativeConfigLoad {
                configs,
                ..NativeConfigLoad::default()
            },
            ConnectOptions {
                profile_data: profile.then(|| self.root.clone()),
                ..ConnectOptions::default()
            },
            Vec::new(),
            ContextLimits::default(),
        )
    }

    async fn connected(&self, revocation: bool) -> McpRuntime {
        let config = self.config();
        self.save(&config, revocation);
        let runtime = self.runtime(vec![config], true);
        runtime.connect(StartupPhase::All).await;
        assert!(
            runtime
                .render_health()
                .contains("state=ready auth=authenticated")
        );
        runtime
    }

    fn initializes(&self) -> Vec<RecordedRequest> {
        self.server
            .requests()
            .into_iter()
            .filter(|request| request.method_name().as_deref() == Some("initialize"))
            .collect()
    }

    fn paths(&self) -> Vec<String> {
        self.server
            .requests()
            .iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .collect()
    }

    fn revocations(&self) -> Vec<String> {
        self.server
            .requests()
            .iter()
            .filter(|request| request.path == "/revoke")
            .map(|request| request.body.clone())
            .collect()
    }
}

type ReloadTask = tokio::task::JoinHandle<Result<ReloadOutcome, super::ReloadCancelled>>;

fn reloading(runtime: &Arc<McpRuntime>, config: McpServerConfig) -> ReloadTask {
    let runtime = Arc::clone(runtime);
    tokio::spawn(async move {
        runtime
            .reconcile(
                NativeConfigLoad {
                    configs: vec![config],
                    ..NativeConfigLoad::default()
                },
                true,
                true,
                &CancellationToken::new(),
            )
            .await
    })
}

fn logging_out(
    runtime: &Arc<McpRuntime>,
) -> tokio::task::JoinHandle<Result<LogoutResult, McpError>> {
    let runtime = Arc::clone(runtime);
    tokio::spawn(async move { runtime.logout_server("remote").await })
}

fn holding_the_second_initialize(
    release: &Arc<Notify>,
) -> impl Fn(&RecordedRequest) -> Reply + Send + Sync + 'static {
    let held = Arc::clone(release);
    let initializes = AtomicUsize::new(0);
    move |request| {
        let answer = reply(request, 200);
        if request.method_name().as_deref() == Some("initialize")
            && initializes.fetch_add(1, Ordering::AcqRel) == 1
        {
            return answer.released_by(&held);
        }
        answer
    }
}

fn logged_out(revocation_failed: bool) -> LogoutResult {
    LogoutResult {
        removed: true,
        revocation_failed,
        repaired_entries: 0,
        local_only: false,
    }
}

#[tokio::test]
async fn a_logout_closes_the_session_deletes_the_grant_and_revokes_it() {
    let fixture = Fixture::start(200).await;
    let runtime = fixture.connected(true).await;
    assert_eq!(runtime.logout_server("remote").await, Ok(logged_out(false)));
    assert!(!fixture.file().exists());
    let paths = fixture.paths();
    let closed = paths.iter().position(|path| path == "DELETE /mcp").unwrap();
    let revoked = paths
        .iter()
        .position(|path| path == "POST /revoke")
        .unwrap();
    assert!(closed < revoked, "{paths:?}");
    let closing = &fixture.server.requests()[closed];
    assert_eq!(
        closing.header("authorization"),
        Some("Bearer stored-access")
    );
    let revocations = fixture.revocations();
    assert_eq!(revocations.len(), 2);
    assert!(revocations[0].contains("token_type_hint=refresh_token"));
    assert!(revocations[1].contains("token_type_hint=access_token"));
    assert!(matches!(runtime.current()[0].lifecycle(), Lifecycle::Idle));
    let health = runtime.render_health();
    assert!(
        health.contains("state=disconnected auth=none status=unavailable\n"),
        "{health}"
    );
    assert!(runtime.tools().is_empty());
    assert_eq!(
        runtime.model_catalog()[0].availability,
        Availability::Unavailable
    );
    let summary = runtime.render_summary();
    assert!(summary.contains("0 ready"), "{summary}");
    for secret in SECRETS {
        assert!(!health.contains(secret));
        assert!(!runtime.render_summary().contains(secret));
    }
    let before = fixture.initializes().len();
    let server = Arc::clone(&runtime.current()[0]);
    assert!(
        server
            .running_client(tokio::time::Instant::now() + Duration::from_secs(1))
            .await
            .is_err()
    );
    assert_eq!(fixture.initializes().len(), before);
    assert_eq!(
        runtime
            .reconnect_authenticated_server("remote", &CancellationToken::new())
            .await,
        Ok(())
    );
    assert!(matches!(
        runtime.current()[0].lifecycle(),
        Lifecycle::Ready(_)
    ));
    let reconnected = fixture.server.requests();
    let last = reconnected
        .iter()
        .rev()
        .find(|request| request.method_name().as_deref() == Some("initialize"))
        .unwrap();
    assert_eq!(last.header("authorization"), None);
}

#[tokio::test]
async fn a_failed_or_missing_revocation_still_logs_out() {
    let failing = Fixture::start(503).await;
    let runtime = failing.connected(true).await;
    assert_eq!(runtime.logout_server("remote").await, Ok(logged_out(true)));
    assert!(!failing.file().exists());
    assert_eq!(failing.revocations().len(), 2);
    let silent = Fixture::start(200).await;
    let runtime = silent.connected(false).await;
    assert_eq!(runtime.logout_server("remote").await, Ok(logged_out(false)));
    assert!(silent.revocations().is_empty());
}

#[tokio::test]
async fn logout_without_stored_credentials_keeps_the_live_server() {
    let fixture = Fixture::start(200).await;
    let runtime = fixture.runtime(vec![fixture.config()], true);
    runtime.connect(StartupPhase::All).await;
    let Lifecycle::Ready(before) = runtime.current()[0].lifecycle() else {
        panic!("the server connected");
    };
    assert_eq!(
        runtime.logout_server("remote").await,
        Ok(LogoutResult::default())
    );
    let Lifecycle::Ready(after) = runtime.current()[0].lifecycle() else {
        panic!("the server stayed connected");
    };
    assert!(Arc::ptr_eq(&before, &after));
    assert!(!fixture.paths().iter().any(|path| path == "DELETE /mcp"));
    let output = call(&runtime, "mcp_remote_echo", "{}").await;
    assert_eq!(output.status, ToolResultStatus::Success);
}

#[tokio::test]
async fn a_store_failure_keeps_the_live_server_and_its_authentication() {
    let fixture = Fixture::start(200).await;
    let runtime = fixture.connected(true).await;
    std::fs::write(fixture.file(), "{").unwrap();
    assert_eq!(
        runtime.logout_server("remote").await,
        Err(McpError::UnexpectedEndOfInput)
    );
    assert!(matches!(
        runtime.current()[0].lifecycle(),
        Lifecycle::Ready(_)
    ));
    assert!(
        runtime
            .render_health()
            .contains("state=ready auth=authenticated")
    );
    let output = call(&runtime, "mcp_remote_echo", "{}").await;
    assert_eq!(output.status, ToolResultStatus::Success);
    assert!(fixture.revocations().is_empty());
}

#[tokio::test]
async fn unreadable_entries_or_a_session_grant_still_log_out() {
    let fixture = Fixture::start(200).await;
    let runtime = fixture.runtime(vec![fixture.config()], true);
    runtime.connect(StartupPhase::All).await;
    let directory = fixture.root.join("mcp-credentials");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(fixture.file(), r#"{"version":1,"credentials":[{}]}"#).unwrap();
    std::fs::set_permissions(
        fixture.file(),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(
        runtime.logout_server("remote").await,
        Ok(LogoutResult {
            removed: true,
            repaired_entries: 1,
            ..LogoutResult::default()
        })
    );
    assert!(matches!(runtime.current()[0].lifecycle(), Lifecycle::Idle));
    let session = Fixture::start(200).await;
    let runtime = session.connected(true).await;
    let auth = &runtime.current()[0].auth;
    auth.publish(
        auth.generation(),
        &CancellationToken::new(),
        session.grant(true),
        |_| Ok(()),
    )
    .unwrap();
    std::fs::remove_file(session.file()).unwrap();
    assert_eq!(runtime.logout_server("remote").await, Ok(logged_out(false)));
    assert_eq!(session.revocations().len(), 2);
}

#[tokio::test]
async fn an_unapproved_project_server_logs_out_locally() {
    let fixture = Fixture::start(200).await;
    let pending = McpServerConfig {
        source: ConfigSource::Workspace,
        workspace_admission: Some(WorkspaceAdmission::Pending),
        ..fixture.config()
    };
    fixture.save(&pending, true);
    let runtime = fixture.runtime(vec![pending], true);
    assert_eq!(
        runtime.logout_server("remote").await,
        Ok(LogoutResult {
            removed: true,
            local_only: true,
            ..LogoutResult::default()
        })
    );
    assert!(!fixture.file().exists());
    assert!(fixture.server.requests().is_empty());
    assert_eq!(
        runtime.logout_server("remote").await,
        Ok(LogoutResult {
            local_only: true,
            ..LogoutResult::default()
        })
    );
}

#[tokio::test]
async fn logout_refuses_unknown_local_profileless_and_starting_servers() {
    let fixture = Fixture::start(200).await;
    let runtime = fixture.runtime(
        vec![McpServerConfig::stdio("local", "/bin/true", Vec::new())],
        true,
    );
    assert_eq!(
        runtime.logout_server("absent").await,
        Err(McpError::McpServerNotFound)
    );
    assert_eq!(
        runtime.logout_server("local").await,
        Err(McpError::McpAuthenticationNotRemote)
    );
    let profileless = fixture.runtime(vec![fixture.config()], false);
    assert_eq!(
        profileless.logout_server("remote").await,
        Err(McpError::HomeNotSet)
    );
    let hanging = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let starting = fixture.runtime(
        vec![McpServerConfig {
            startup_timeout_ms: 5_000,
            ..McpServerConfig::remote(
                "remote",
                TransportType::Http,
                format!("http://{}/mcp", hanging.local_addr().unwrap()),
            )
        }],
        true,
    );
    let settling = starting.connect(StartupPhase::All);
    for _ in 0..200 {
        if matches!(starting.current()[0].lifecycle(), Lifecycle::Starting) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(matches!(
        starting.current()[0].lifecycle(),
        Lifecycle::Starting
    ));
    assert_eq!(
        starting.logout_server("remote").await,
        Err(McpError::McpDiscoveryInProgress)
    );
    settling.abandon().await;
}

#[tokio::test]
async fn a_server_that_needs_authentication_logs_out_and_drops_its_challenge() {
    let server = FakeServer::start(|request| {
        if request.path == "/revoke" {
            return Reply::status(200);
        }
        Reply::status(401).header("WWW-Authenticate", r#"Bearer scope="tools.call""#)
    })
    .await;
    let fixture = Fixture::serving(server);
    let config = fixture.config();
    fixture.save(&config, true);
    let runtime = fixture.runtime(vec![config], true);
    runtime.connect(StartupPhase::All).await;
    let health = runtime.render_health();
    assert!(
        health.contains("auth=required status=needs_auth"),
        "{health}"
    );
    assert_eq!(runtime.logout_server("remote").await, Ok(logged_out(false)));
    let health = runtime.render_health();
    assert!(
        health.contains("state=disconnected auth=none status=unavailable\n"),
        "{health}"
    );
    assert_eq!(
        runtime.current()[0].auth.pending(),
        crate::mcp_auth::Challenge::default()
    );
    assert_eq!(fixture.revocations().len(), 2);
}

#[tokio::test]
async fn a_logout_during_a_reload_signs_out_the_server_the_reload_publishes() {
    let release = Arc::new(Notify::new());
    let fixture =
        Fixture::serving(FakeServer::start(holding_the_second_initialize(&release)).await);
    let runtime = Arc::new(fixture.connected(false).await);
    let original = Arc::clone(&runtime.current()[0]);
    let changed = McpServerConfig {
        operation_timeout_ms: 7_000,
        ..fixture.config()
    };
    let reload = reloading(&runtime, changed);
    for _ in 0..500 {
        if fixture.initializes().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let initializes = fixture.initializes();
    assert_eq!(initializes.len(), 2);
    assert_eq!(
        initializes[1].header("authorization"),
        Some("Bearer stored-access")
    );
    let logout = logging_out(&runtime);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!logout.is_finished());
    assert!(fixture.file().exists());
    release.notify_one();
    assert!(matches!(
        reload.await.unwrap(),
        Ok(ReloadOutcome::Published { .. })
    ));
    assert_eq!(logout.await.unwrap(), Ok(logged_out(false)));
    assert!(!fixture.file().exists());
    let replacement = Arc::clone(&runtime.current()[0]);
    assert!(!Arc::ptr_eq(&original, &replacement));
    assert_eq!(replacement.config.operation_timeout_ms, 7_000);
    assert!(matches!(replacement.lifecycle(), Lifecycle::Idle));
    let health = runtime.render_health();
    assert!(
        health.contains("state=disconnected auth=none status=unavailable\n"),
        "{health}"
    );
    assert!(runtime.tools().is_empty());
    let signed_out = fixture.server.requests().len();
    let _ = runtime.list_prompts("remote").await;
    let later = &fixture.server.requests()[signed_out..];
    assert!(
        later
            .iter()
            .all(|request| request.header("authorization").is_none()),
        "{later:?}"
    );
    assert!(!runtime.render_health().contains("auth=authenticated"));
}

#[tokio::test]
async fn a_reload_during_a_logout_connects_only_after_the_grant_is_gone() {
    let fixture = Fixture::start(200).await;
    let runtime = Arc::new(fixture.connected(false).await);
    let held = ofx_config::PrivateDir::open_or_create(&fixture.root)
        .unwrap()
        .open_or_create_child("mcp-credentials")
        .unwrap()
        .try_lock("credentials.lock")
        .unwrap()
        .unwrap();
    let logout = logging_out(&runtime);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!logout.is_finished());
    let changed = McpServerConfig {
        operation_timeout_ms: 7_000,
        ..fixture.config()
    };
    let reload = reloading(&runtime, changed);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!reload.is_finished());
    assert_eq!(fixture.initializes().len(), 1);
    drop(held);
    assert_eq!(logout.await.unwrap(), Ok(logged_out(false)));
    assert!(matches!(
        reload.await.unwrap(),
        Ok(ReloadOutcome::Published { .. })
    ));
    assert!(!fixture.file().exists());
    let initializes = fixture.initializes();
    assert_eq!(initializes.len(), 2);
    assert_eq!(initializes[1].header("authorization"), None);
    let health = runtime.render_health();
    assert!(health.contains("state=ready"), "{health}");
    assert!(!health.contains("auth=authenticated"), "{health}");
    let output = call(&runtime, "mcp_remote_echo", "{}").await;
    assert_eq!(output.status, ToolResultStatus::Success);
    let calls: Vec<RecordedRequest> = fixture
        .server
        .requests()
        .into_iter()
        .filter(|request| request.method_name().as_deref() == Some("tools/call"))
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].header("authorization"), None);
}
