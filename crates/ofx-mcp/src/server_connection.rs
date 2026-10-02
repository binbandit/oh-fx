use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::error::McpError;
use crate::features::tools::ToolCatalog;
use crate::mcp_contract::{ConfigSource, McpServerConfig, TransportType, WorkspaceAdmission};
use crate::protocol_negotiation::ElicitationWire;
use crate::server_transport::{
    ConnectOptions, Connected, ServerInfo, StartupFailure, connect_http, connect_sse, connect_stdio,
};
use crate::transport::{McpTransport, ShutdownMode};

#[derive(Debug, Clone, PartialEq)]
pub enum ServerNotification {
    ToolsListChanged,
    ResourcesListChanged,
    PromptsListChanged,
    ResourceUpdated {
        uri: String,
    },
    Other {
        method: String,
        params: Option<Value>,
    },
}

pub struct McpClient {
    pub(crate) name: String,
    pub(crate) transport: Box<dyn McpTransport>,
    pub(crate) info: ServerInfo,
    pub(crate) wire: Option<ElicitationWire>,
    pub(crate) operation_timeout: Duration,
    pub(crate) catalog: Mutex<Arc<ToolCatalog>>,
    pub(crate) tools_stale: AtomicBool,
    notifications: Mutex<mpsc::UnboundedReceiver<Value>>,
    received: Mutex<Vec<ServerNotification>>,
}

impl McpClient {
    pub async fn connect(
        config: &McpServerConfig,
        options: &ConnectOptions,
    ) -> Result<Self, StartupFailure> {
        if config.source == ConfigSource::Workspace
            && config.workspace_admission != Some(WorkspaceAdmission::Approved)
        {
            return Err(StartupFailure::from(McpError::McpWorkspaceApprovalRequired));
        }
        let connected = match config.transport {
            TransportType::Stdio => connect_stdio(config, options).await?,
            TransportType::Http => connect_http(config, options).await?,
            TransportType::Sse => connect_sse(config, options).await?,
        };
        Ok(Self::from_connected(config, connected))
    }

    fn from_connected(config: &McpServerConfig, connected: Connected) -> Self {
        Self {
            name: config.name.clone(),
            transport: connected.transport,
            info: connected.info,
            wire: connected.wire,
            operation_timeout: Duration::from_millis(config.operation_timeout_ms.into()),
            catalog: Mutex::new(Arc::new(connected.catalog)),
            tools_stale: AtomicBool::new(false),
            notifications: Mutex::new(connected.notifications),
            received: Mutex::new(Vec::new()),
        }
    }

    pub fn server_name(&self) -> &str {
        &self.name
    }

    pub fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    pub fn is_running(&self) -> bool {
        self.transport.is_running()
    }

    pub fn tool_catalog(&self) -> Arc<ToolCatalog> {
        Arc::clone(&lock(&self.catalog))
    }

    pub fn poll_notifications(&self) -> Vec<ServerNotification> {
        self.receive_notifications();
        std::mem::take(&mut *lock(&self.received))
    }

    pub(crate) fn receive_notifications(&self) {
        let mut notifications = lock(&self.notifications);
        while let Ok(value) = notifications.try_recv() {
            if let Some(notification) = self.classify_notification(&value) {
                lock(&self.received).push(notification);
            }
        }
    }

    fn classify_notification(&self, value: &Value) -> Option<ServerNotification> {
        let method = value.get("method").and_then(Value::as_str)?.to_owned();
        let capabilities = self.info.capabilities;
        match method.as_str() {
            "notifications/tools/list_changed" => {
                if !capabilities.tools_list_changed {
                    return None;
                }
                self.tools_stale.store(true, Ordering::Release);
                Some(ServerNotification::ToolsListChanged)
            }
            "notifications/resources/list_changed" => capabilities
                .resources
                .is_some_and(|resources| resources.list_changed)
                .then_some(ServerNotification::ResourcesListChanged),
            "notifications/prompts/list_changed" => capabilities
                .prompts
                .is_some_and(|prompts| prompts.list_changed)
                .then_some(ServerNotification::PromptsListChanged),
            "notifications/resources/updated"
                if capabilities
                    .resources
                    .is_some_and(|resources| resources.subscribe) =>
            {
                let uri = value.get("params")?.get("uri")?.as_str()?.to_owned();
                Some(ServerNotification::ResourceUpdated { uri })
            }
            _ => Some(ServerNotification::Other {
                method,
                params: value.get("params").cloned(),
            }),
        }
    }

    pub async fn shutdown(self, mode: ShutdownMode) {
        self.transport.shutdown(mode).await;
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::features::tools::{ToolCallOutcome, ToolContent};
    use crate::tool_operations::CallOptions;
    use crate::transport::Progress;

    const SERVER_LOOP: &str = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{\"listChanged\":true}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"},\"instructions\":\"Use the fixture.\"}" ;;
    *'"method":"notifications/initialized"'*) printf '%s\n' "$line" >&2 ;;
    *'"method":"tools/list"'*'"cursor":"page-2"'*)
      reply "$id" '{"tools":[{"name":"alpha","description":"First","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}' ;;
    *'"method":"tools/list"'*)
      if [ -f "$STATE/changed" ]; then
        reply "$id" '{"tools":[{"name":"gamma","inputSchema":{"type":"object"}}]}'
      else
        reply "$id" '{"tools":[{"name":"zeta","inputSchema":{"type":"object"}}],"nextCursor":"page-2"}'
      fi ;;
    *'"method":"tools/call"'*'"name":"roots"'*)
      printf '{"jsonrpc":"2.0","id":"server-1","method":"roots/list","params":{}}\n'
      IFS= read -r answer
      printf '%s\n' "$answer" >&2
      reply "$id" '{"content":[{"type":"text","text":"asked"}]}' ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":%s,"progress":1,"total":2,"message":"half"}}\n' "$id"
      touch "$STATE/changed"
      printf '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}\n'
      reply "$id" '{"content":[{"type":"text","text":"called"},{"type":"image","data":"AA==","mimeType":"image/png"}],"structuredContent":{"ok":true}}' ;;
  esac
done
"#;

    fn script_config(name: &str, script: &str, state: &Path) -> McpServerConfig {
        let mut config =
            McpServerConfig::stdio(name, "/bin/sh", vec!["-c".to_owned(), script.to_owned()]);
        config.env.push(crate::mcp_contract::EnvVar {
            key: "STATE".to_owned(),
            value: state.to_string_lossy().into_owned(),
        });
        config
    }

    fn process_state(pid: i32) -> String {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn process_is_live(pid: i32) -> bool {
        let state = process_state(pid);
        !state.is_empty() && !state.starts_with('Z')
    }

    fn stderr_text(diagnostics: Option<&crate::stdio_dispatcher::ChildDiagnostics>) -> String {
        diagnostics
            .map(|diagnostics| String::from_utf8_lossy(diagnostics.stderr.head()).into_owned())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn stdio_startup_negotiates_discovers_and_calls_tools() {
        let state = tempfile::tempdir().unwrap();
        let config = script_config("fixture", SERVER_LOOP, state.path());
        let client = McpClient::connect(&config, &ConnectOptions::default())
            .await
            .unwrap();
        let info = client.server_info();
        assert_eq!(info.protocol_version, "2025-11-25");
        assert_eq!(info.name.as_deref(), Some("fixture"));
        assert_eq!(info.instructions.as_deref(), Some("Use the fixture."));
        assert!(info.capabilities.tools_list_changed);
        let names: Vec<_> = client
            .tool_catalog()
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);

        let progress = Arc::new(Mutex::new(Vec::new()));
        let sink_progress = Arc::clone(&progress);
        let outcome = client
            .call_tool(
                "alpha",
                &json!({"text": "hi\nthere"}),
                CallOptions {
                    progress: Some(Arc::new(move |update| lock(&sink_progress).push(update))),
                    ..CallOptions::default()
                },
            )
            .await
            .unwrap();
        let ToolCallOutcome::Complete(result) = outcome else {
            panic!("expected a complete call result");
        };
        assert_eq!(
            result.content[0],
            ToolContent::Text {
                text: "called".to_owned()
            }
        );
        assert_eq!(result.structured_content, Some(json!({"ok": true})));
        assert_eq!(
            *lock(&progress),
            vec![Progress {
                progress: 1.0,
                total: Some(2.0),
                message: Some("half".to_owned()),
            }]
        );

        let refreshed = client.current_tools().await.unwrap();
        assert_eq!(refreshed.tools[0].name, "gamma");
        assert_eq!(client.tool_catalog().tools.len(), 1);
        client.shutdown(ShutdownMode::Graceful).await;
    }

    #[tokio::test]
    async fn stdio_tool_calls_refuse_server_requests_with_method_not_found() {
        let state = tempfile::tempdir().unwrap();
        let config = script_config("fixture", SERVER_LOOP, state.path());
        let client = McpClient::connect(&config, &ConnectOptions::default())
            .await
            .unwrap();
        let outcome = client
            .call_tool("roots", &json!({}), CallOptions::default())
            .await
            .unwrap();
        assert!(matches!(outcome, ToolCallOutcome::Complete(_)));
        client.shutdown(ShutdownMode::Graceful).await;
    }

    #[tokio::test]
    async fn legacy_ladder_relaunches_with_the_hinted_older_version() {
        let state = tempfile::tempdir().unwrap();
        let script = format!(
            r#"IFS= read -r line
case "$line" in
  *'"protocolVersion":"2025-11-25"'*)
    printf '%s\n' "$line" >> "$STATE/offers"
    printf '{{"jsonrpc":"2.0","id":0,"error":{{"code":-32602,"message":"Unsupported protocol version","data":{{"supported":["2025-03-26"],"requested":"2025-11-25"}}}}}}\n'
    cat >/dev/null; exit 0 ;;
esac
printf '%s\n' "$line" >> "$STATE/offers"
printf '%s\n' "$line" | {{ {SERVER_LOOP} }}
{SERVER_LOOP}"#
        );
        let config = script_config("fixture", &script, state.path());
        let client = McpClient::connect(&config, &ConnectOptions::default())
            .await
            .unwrap();
        assert_eq!(client.server_info().protocol_version, "2025-03-26");
        let offers = fs::read_to_string(state.path().join("offers")).unwrap();
        assert_eq!(offers.lines().count(), 2);
        assert!(
            offers
                .lines()
                .nth(1)
                .unwrap()
                .contains("\"capabilities\":{}")
        );
        client.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn legacy_ladder_relaunches_after_each_closed_connection() {
        let state = tempfile::tempdir().unwrap();
        let script = format!(
            r#"IFS= read -r line
printf '%s\n' "$line" >> "$STATE/offers"
case "$line" in
  *'"protocolVersion":"2024-11-05"'*) ;;
  *) exit 0 ;;
esac
printf '%s\n' "$line" | {{ {SERVER_LOOP} }}
{SERVER_LOOP}"#
        );
        let config = script_config("fixture", &script, state.path());
        let client = McpClient::connect(&config, &ConnectOptions::default())
            .await
            .unwrap();
        assert_eq!(client.server_info().protocol_version, "2024-11-05");
        let offers = fs::read_to_string(state.path().join("offers")).unwrap();
        let versions: Vec<_> = offers
            .lines()
            .map(|line| line.split("\"protocolVersion\":\"").nth(1).unwrap()[..10].to_owned())
            .collect();
        assert_eq!(
            versions,
            ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"]
        );
        client.shutdown(ShutdownMode::Graceful).await;
    }

    #[tokio::test]
    async fn a_server_that_exits_at_every_version_reports_its_exit_without_restarting() {
        let state = tempfile::tempdir().unwrap();
        let script = r#"echo launch >> "$STATE/launches"; echo 'fatal: missing token' >&2; exit 3"#;
        let mut config = script_config("fixture", script, state.path());
        config.restart_limit = 2;
        let failure = McpClient::connect(&config, &ConnectOptions::default())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error, McpError::McpServerExitedDuringStartup);
        let diagnostics = failure.diagnostics.unwrap();
        assert_eq!(diagnostics.status.and_then(|status| status.code()), Some(3));
        assert_eq!(diagnostics.stderr.head(), b"fatal: missing token\n");
        let launches = fs::read_to_string(state.path().join("launches")).unwrap();
        assert_eq!(launches.lines().count(), 4);
    }

    #[tokio::test]
    async fn non_mcp_stdout_fails_startup_and_restarts_within_the_limit() {
        let state = tempfile::tempdir().unwrap();
        let script = r#"echo launch >> "$STATE/launches"; echo 'Server banner v1'; cat >/dev/null"#;
        let config = script_config("fixture", script, state.path());
        let failure = McpClient::connect(&config, &ConnectOptions::default())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error, McpError::McpInitFailed);
        assert_eq!(
            failure.diagnostics.unwrap().rejected_output.unwrap().bytes,
            b"Server banner v1"
        );
        let launches = fs::read_to_string(state.path().join("launches")).unwrap();
        assert_eq!(launches.lines().count(), 2);
    }

    #[tokio::test]
    async fn startup_shares_one_deadline_and_reports_a_timeout() {
        let state = tempfile::tempdir().unwrap();
        let script = r"echo 'starting slowly' >&2; cat >/dev/null";
        let mut config = script_config("fixture", script, state.path());
        config.startup_timeout_ms = 300;
        let started = std::time::Instant::now();
        let failure = McpClient::connect(&config, &ConnectOptions::default())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error, McpError::McpRequestTimedOut);
        assert!(stderr_text(failure.diagnostics.as_ref()).contains("starting slowly"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn one_invalid_tool_definition_fails_discovery() {
        let state = tempfile::tempdir().unwrap();
        let script = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{}}}\n' "$id" ;;
    *'"method":"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"good","inputSchema":{"type":"object"}},{"name":"bad","inputSchema":{"type":"array"}}]}}\n' "$id" ;;
  esac
done"#;
        let mut config = script_config("fixture", script, state.path());
        config.restart_limit = 0;
        let failure = McpClient::connect(&config, &ConnectOptions::default())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error, McpError::InvalidSchema);
    }

    #[tokio::test]
    async fn graceful_shutdown_closes_stdin_and_kills_the_whole_process_group() {
        let state = tempfile::tempdir().unwrap();
        let script = format!(
            r#"sleep 30 &
echo $! > "$STATE/descendant"
{SERVER_LOOP}"#
        );
        let config = script_config("fixture", &script, state.path());
        let client = McpClient::connect(&config, &ConnectOptions::default())
            .await
            .unwrap();
        let descendant: i32 = fs::read_to_string(state.path().join("descendant"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        client.shutdown(ShutdownMode::Graceful).await;
        let mut gone = false;
        for _ in 0..50 {
            if !process_is_live(descendant) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "{:?}", process_state(descendant));
    }

    #[tokio::test]
    async fn docker_run_servers_get_a_cidfile_that_shutdown_cleans_up() {
        let state = tempfile::tempdir().unwrap();
        let bin = state.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let docker = bin.join("docker");
        fs::write(
            &docker,
            format!(
                r#"#!/bin/sh
if [ "$1" = "rm" ]; then
  echo "$*" > "$STATE/removed"
  exit 0
fi
printf '0123456789abcdef0123\n' > "$3"
printf '%s\n' "$3" > "$STATE/cidfile"
{SERVER_LOOP}"#
            ),
        )
        .unwrap();
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = McpServerConfig::stdio(
            "docker",
            docker.to_string_lossy(),
            vec!["run".to_owned(), "-i".to_owned(), "image".to_owned()],
        );
        config.env.push(crate::mcp_contract::EnvVar {
            key: "STATE".to_owned(),
            value: state.path().to_string_lossy().into_owned(),
        });
        let client = McpClient::connect(&config, &ConnectOptions::default())
            .await
            .unwrap();
        let cidfile = fs::read_to_string(state.path().join("cidfile")).unwrap();
        let cidfile = cidfile.trim();
        assert!(Path::new(cidfile).exists());
        client.shutdown(ShutdownMode::Graceful).await;
        assert_eq!(
            fs::read_to_string(state.path().join("removed"))
                .unwrap()
                .trim(),
            "rm -f 0123456789abcdef0123"
        );
        assert!(!Path::new(cidfile).exists());
    }

    #[tokio::test]
    async fn workspace_servers_never_launch_without_approval() {
        let state = tempfile::tempdir().unwrap();
        let script = r#"touch "$STATE/launched""#;
        for admission in [WorkspaceAdmission::Pending, WorkspaceAdmission::Rejected] {
            let config = McpServerConfig {
                source: ConfigSource::Workspace,
                scope: crate::mcp_contract::ConfigScope::Workspace,
                workspace_admission: Some(admission),
                ..script_config("project", script, state.path())
            };
            let failure = McpClient::connect(&config, &ConnectOptions::default())
                .await
                .err()
                .unwrap();
            assert_eq!(failure.error, McpError::McpWorkspaceApprovalRequired);
        }
        assert!(!state.path().join("launched").exists());
    }

    #[tokio::test]
    async fn notifications_are_filtered_by_advertised_capabilities() {
        let state = tempfile::tempdir().unwrap();
        let config = script_config("fixture", SERVER_LOOP, state.path());
        let client = McpClient::connect(&config, &ConnectOptions::default())
            .await
            .unwrap();
        assert_eq!(
            client.classify_notification(
                &json!({"jsonrpc":"2.0","method":"notifications/resources/list_changed"})
            ),
            None
        );
        assert_eq!(
            client.classify_notification(
                &json!({"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info"}})
            ),
            Some(ServerNotification::Other {
                method: "notifications/message".to_owned(),
                params: Some(json!({"level": "info"})),
            })
        );
        assert!(client.poll_notifications().is_empty());
        client
            .call_tool("alpha", &json!({}), CallOptions::default())
            .await
            .unwrap();
        client.current_tools().await.unwrap();
        assert_eq!(
            client.poll_notifications(),
            vec![ServerNotification::ToolsListChanged]
        );
        client.shutdown(ShutdownMode::Immediate).await;
    }
}
