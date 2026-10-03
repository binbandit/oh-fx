use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use ofx_contract::{DynamicTools, Tool};
use tokio::task::JoinHandle;

use crate::mcp_contract::{ConfigSource, ProfileConfigWarning, WorkspaceAdmission};
use crate::native_config::NativeConfigLoad;
use crate::project_config::WorkspaceDiagnostic;
use crate::server_lifecycle::{Server, ServerStatus};
use crate::server_transport::ConnectOptions;
use crate::startup_admission::{StartupDecision, StartupPhase, decide_startup};
use crate::timing::spawn;
use crate::tool_mcp_registry::{SchemaLimits, publish_tools};
use crate::tool_names::ToolNames;
use crate::transport::ShutdownMode;

pub struct McpRuntime {
    servers: Vec<Arc<Server>>,
    names: Mutex<ToolNames>,
    reserved: Vec<String>,
    limits: SchemaLimits,
    catalog_generation: Arc<AtomicU64>,
    published: Mutex<Published>,
    workspace_diagnostics: Vec<WorkspaceDiagnostic>,
    profile_warning: Option<ProfileConfigWarning>,
}

#[derive(Default)]
struct Published {
    generation: Option<u64>,
    tools: Vec<Arc<dyn Tool>>,
    notices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerView {
    pub name: String,
    pub required: bool,
    pub workspace_admission: Option<WorkspaceAdmission>,
    pub enabled: bool,
    pub status: ServerStatus,
}

impl McpRuntime {
    pub fn new(
        load: NativeConfigLoad,
        options: &ConnectOptions,
        reserved: Vec<String>,
        limits: SchemaLimits,
    ) -> Self {
        let catalog_generation = Arc::new(AtomicU64::new(0));
        let servers = load
            .configs
            .into_iter()
            .map(|config| {
                Arc::new(Server::new(
                    config,
                    options.clone(),
                    Arc::clone(&catalog_generation),
                ))
            })
            .collect();
        Self {
            servers,
            names: Mutex::new(ToolNames::default()),
            reserved,
            limits,
            catalog_generation,
            published: Mutex::new(Published::default()),
            workspace_diagnostics: load.workspace_diagnostics,
            profile_warning: load.profile_warning,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn connect(&self, phase: StartupPhase) -> Settling {
        Settling::all(
            self.servers
                .iter()
                .filter(|server| decide_startup(&server.config, phase) == StartupDecision::Connect)
                .map(|server| (Arc::clone(server), Step::Start)),
        )
    }

    pub fn servers(&self) -> Vec<ServerView> {
        self.servers
            .iter()
            .map(|server| ServerView {
                name: server.config.name.clone(),
                required: server.config.required,
                workspace_admission: server.config.workspace_admission,
                enabled: server.config.enabled,
                status: server.status(),
            })
            .collect()
    }

    pub fn pending_workspace_names(&self) -> Vec<String> {
        self.servers
            .iter()
            .filter(|server| {
                server.config.source == ConfigSource::Workspace
                    && server.config.enabled
                    && server.config.workspace_admission == Some(WorkspaceAdmission::Pending)
            })
            .map(|server| server.config.name.clone())
            .collect()
    }

    pub fn required_startup_failure(&self) -> Option<String> {
        let unavailable = self
            .servers
            .iter()
            .filter(|server| server.config.required && server.config.enabled)
            .find(|server| !matches!(server.status(), ServerStatus::Ready { .. }))?;
        let failure = match unavailable.status() {
            ServerStatus::Failed(message) => message,
            _ => "Check the trusted profile configuration and retry.".to_owned(),
        };
        Some(format!(
            "Required MCP server '{}' failed to start: {failure}",
            unavailable.config.name
        ))
    }

    pub fn workspace_diagnostics(&self) -> &[WorkspaceDiagnostic] {
        &self.workspace_diagnostics
    }

    pub fn profile_warning(&self) -> Option<&ProfileConfigWarning> {
        self.profile_warning.as_ref()
    }

    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut lock(&self.published).notices)
    }

    pub fn shutdown(&self, mode: ShutdownMode) -> Settling {
        Settling::all(
            self.servers
                .iter()
                .map(|server| (Arc::clone(server), Step::Stop(mode))),
        )
    }
}

#[derive(Debug, Clone, Copy)]
enum Step {
    Start,
    Stop(ShutdownMode),
}

pub struct Settling(Vec<JoinHandle<()>>);

impl Settling {
    fn all(steps: impl Iterator<Item = (Arc<Server>, Step)>) -> Self {
        Self(
            steps
                .map(|(server, step)| spawn(settle(server, step)))
                .collect(),
        )
    }
}

impl Future for Settling {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        while let Some(task) = self.0.last_mut() {
            if Pin::new(task).poll(context).is_pending() {
                return Poll::Pending;
            }
            self.0.pop();
        }
        Poll::Ready(())
    }
}

impl Drop for Settling {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

async fn settle(server: Arc<Server>, step: Step) {
    match step {
        Step::Start => server.start().await,
        Step::Stop(mode) => server.stop(mode).await,
    }
}

impl DynamicTools for McpRuntime {
    fn generation(&self) -> u64 {
        self.catalog_generation.load(Ordering::Acquire)
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let generation = self.generation();
        let mut published = lock(&self.published);
        if published.generation != Some(generation) {
            let mut names = lock(&self.names);
            let (tools, notices) =
                publish_tools(&self.servers, &mut names, &self.reserved, self.limits);
            published.generation = Some(generation);
            published.tools = tools;
            published.notices.extend(notices);
        }
        published.tools.clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use ofx_config::ContextLimitName;
    use ofx_config::ContextLimits;
    use ofx_contract::{PathAccess, ToolContext, ToolResultStatus};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::mcp_contract::{ConfigScope, EnvVar, McpServerConfig};

    const SERVER: &str = r#"
echo $$ >> "$STATE/pids"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"tools\":{\"listChanged\":true}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"},\"instructions\":\"Prefer alpha.\"}" ;;
    *'"method":"tools/list"'*)
      if [ -f "$STATE/changed" ]; then
        reply "$id" '{"tools":[{"name":"alpha","description":"First","inputSchema":{"type":"object"}},{"name":"change","inputSchema":{"type":"object"}},{"name":"crash","inputSchema":{"type":"object"}},{"name":"beta","inputSchema":{"type":"object"}}]}'
      else
        reply "$id" '{"tools":[{"name":"alpha","description":"First","inputSchema":{"type":"object"}},{"name":"change","inputSchema":{"type":"object"}},{"name":"crash","inputSchema":{"type":"object"}}]}'
      fi ;;
    *'"name":"change"'*)
      touch "$STATE/changed"
      printf '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}\n'
      reply "$id" '{"content":[{"type":"text","text":"changed"}]}' ;;
    *'"name":"crash"'*) exit 7 ;;
    *'"method":"tools/call"'*)
      reply "$id" '{"content":[{"type":"text","text":"called"}]}' ;;
  esac
done
"#;

    fn config(name: &str, script: &str, state: &Path) -> McpServerConfig {
        let mut config =
            McpServerConfig::stdio(name, "/bin/sh", vec!["-c".to_owned(), script.to_owned()]);
        config.env.push(EnvVar {
            key: "STATE".to_owned(),
            value: state.to_string_lossy().into_owned(),
        });
        config
    }

    fn limits() -> SchemaLimits {
        let limits = ContextLimits::default();
        SchemaLimits {
            server_instructions: limits.get(ContextLimitName::McpServerInstructionsBytes),
            selected_schema: limits.get(ContextLimitName::McpSelectedSchemaBytes),
        }
    }

    fn runtime(configs: Vec<McpServerConfig>) -> Arc<McpRuntime> {
        Arc::new(McpRuntime::new(
            NativeConfigLoad {
                configs,
                ..NativeConfigLoad::default()
            },
            &ConnectOptions::default(),
            vec!["read_file".to_owned()],
            limits(),
        ))
    }

    fn names(runtime: &McpRuntime) -> Vec<String> {
        runtime
            .tools()
            .iter()
            .map(|tool| tool.spec().name.clone())
            .collect()
    }

    async fn call(runtime: &McpRuntime, name: &str, arguments: &str) -> ofx_contract::ToolOutput {
        let tool = runtime
            .tools()
            .into_iter()
            .find(|tool| tool.spec().name == name)
            .expect("an advertised tool");
        let Ok(prepared) = tool.prepare(arguments) else {
            panic!("arguments were refused");
        };
        assert!(prepared.mcp_tool());
        prepared
            .execute(ToolContext::new(
                ofx_contract::ToolCallId::new("call"),
                CancellationToken::new(),
                PathAccess::WorkspaceOnly,
            ))
            .await
    }

    fn process_ended(pid: i32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, fields)| fields.starts_with('Z'))
        })
    }

    #[tokio::test]
    async fn connecting_publishes_prefixed_tools_with_server_instructions() {
        let state = tempfile::tempdir().unwrap();
        let disabled = McpServerConfig {
            enabled: false,
            ..config("off", SERVER, state.path())
        };
        let runtime = runtime(vec![config("fixture", SERVER, state.path()), disabled]);
        assert_eq!(runtime.generation(), 0);
        runtime.connect(StartupPhase::All).await;
        assert!(runtime.generation() > 0);
        assert_eq!(
            names(&runtime),
            [
                "mcp_fixture_alpha",
                "mcp_fixture_change",
                "mcp_fixture_crash"
            ]
        );
        let spec = runtime.tools()[0].spec().clone();
        assert_eq!(
            spec.description,
            "First\n\nServer instructions: Prefer alpha."
        );
        assert_eq!(spec.input_schema, r#"{"type":"object"}"#);
        let Ok(prepared) = runtime.tools()[0].prepare("{}") else {
            panic!("arguments were refused");
        };
        assert_eq!(
            prepared.review_schema().as_deref(),
            Some(
                r#"{"type":"function","name":"mcp_fixture_alpha","description":"First\n\nServer instructions: Prefer alpha.","inputSchema":{"type":"object"}}"#
            )
        );
        let statuses: Vec<_> = runtime
            .servers()
            .into_iter()
            .map(|server| server.status)
            .collect();
        assert_eq!(
            statuses,
            [ServerStatus::Ready { tools: 3 }, ServerStatus::Waiting]
        );
        let output = call(&runtime, "mcp_fixture_alpha", "{}").await;
        assert_eq!(output.status, ToolResultStatus::Success);
        assert_eq!(
            output.content,
            r#"{"server":"fixture","tool":"alpha","result":{"content":[{"type":"text","text":"called"}]}}"#
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_tool_list_change_republishes_the_server_tools() {
        let state = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![config("fixture", SERVER, state.path())]);
        runtime.connect(StartupPhase::All).await;
        let before = runtime.generation();
        assert_eq!(names(&runtime).len(), 3);
        let output = call(&runtime, "mcp_fixture_change", "{}").await;
        assert_eq!(output.status, ToolResultStatus::Success);
        for _ in 0..200 {
            if runtime.generation() > before && names(&runtime).len() == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(names(&runtime).contains(&"mcp_fixture_beta".to_owned()));
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_crashed_server_restarts_once_then_reports_the_limit() {
        let state = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![config("fixture", SERVER, state.path())]);
        runtime.connect(StartupPhase::All).await;
        let crashed = call(&runtime, "mcp_fixture_crash", "{}").await;
        assert_eq!(crashed.status, ToolResultStatus::Failure);
        let output = call(&runtime, "mcp_fixture_alpha", "{}").await;
        assert_eq!(
            output.status,
            ToolResultStatus::Success,
            "{}",
            output.content
        );
        call(&runtime, "mcp_fixture_crash", "{}").await;
        let output = call(&runtime, "mcp_fixture_alpha", "{}").await;
        assert_eq!(output.status, ToolResultStatus::Failure);
        assert!(
            output.content.contains("McpRestartLimitReached"),
            "{}",
            output.content
        );
        assert_eq!(
            runtime.servers()[0].status,
            ServerStatus::Failed("MCP restart limit reached".to_owned())
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn shutdown_ends_every_server_process() {
        let state = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![
            config("one", SERVER, state.path()),
            config("two", SERVER, state.path()),
        ]);
        runtime.connect(StartupPhase::All).await;
        runtime.shutdown(ShutdownMode::Immediate).await;
        let pids = std::fs::read_to_string(state.path().join("pids")).unwrap();
        for pid in pids.lines() {
            assert!(process_ended(pid.parse().unwrap()), "{pid}");
        }
        assert!(runtime.tools().is_empty());
    }

    #[tokio::test]
    async fn unapproved_workspace_servers_are_listed_and_never_started() {
        let state = tempfile::tempdir().unwrap();
        let pending = McpServerConfig {
            source: ConfigSource::Workspace,
            scope: ConfigScope::Workspace,
            workspace_admission: Some(WorkspaceAdmission::Pending),
            ..config("project", SERVER, state.path())
        };
        let runtime = runtime(vec![pending]);
        runtime.connect(StartupPhase::All).await;
        assert_eq!(runtime.pending_workspace_names(), ["project"]);
        assert!(!state.path().join("pids").exists());
        assert_eq!(runtime.servers()[0].status, ServerStatus::Waiting);
    }

    #[tokio::test]
    async fn a_required_server_that_exits_names_its_failure() {
        let state = tempfile::tempdir().unwrap();
        let broken = McpServerConfig {
            required: true,
            restart_limit: 0,
            ..config("broken", "echo 'boom' >&2; exit 3", state.path())
        };
        let runtime = runtime(vec![broken]);
        runtime.connect(StartupPhase::All).await;
        assert_eq!(
            runtime.required_startup_failure().as_deref(),
            Some(
                "Required MCP server 'broken' failed to start: MCP server exited with code 3 before completing startup: boom"
            )
        );
    }

    #[tokio::test]
    async fn oversized_schemas_are_left_out_with_a_context_notice() {
        let state = tempfile::tempdir().unwrap();
        let runtime = Arc::new(McpRuntime::new(
            NativeConfigLoad {
                configs: vec![config("fixture", SERVER, state.path())],
                ..NativeConfigLoad::default()
            },
            &ConnectOptions::default(),
            Vec::new(),
            SchemaLimits {
                selected_schema: ofx_config::ContextLimit {
                    value: ofx_config::ContextLimitValue::Bytes(16),
                    source: ofx_config::ContextLimitSource::CommandLine,
                },
                ..limits()
            },
        ));
        runtime.connect(StartupPhase::All).await;
        assert!(runtime.tools().is_empty());
        let notices = runtime.take_notices();
        assert_eq!(notices.len(), 3);
        assert!(
            notices[0]
                .starts_with("[context] MCP schema \"mcp_fixture_alpha\" rejected: observed=")
        );
        assert!(notices[0].ends_with(
            "effective=16 bytes source=command line; override with --context-limit mcp_selected_schema_bytes=BYTES|off"
        ));
        runtime.shutdown(ShutdownMode::Immediate).await;
    }
}
