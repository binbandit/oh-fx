use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use ofx_config::{ContextLimitName, ContextLimitValue, ContextLimits};
use ofx_contract::{
    BoxFuture, DynamicTools, McpSearchHost, McpSearchRequest, McpSearchResult, McpToolSearch, Tool,
};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::error::McpError;
use crate::feature_operations::{FeatureFailure, PromptSummary, ResourceSummary};
use crate::features::common::ResourceContent;
use crate::features::completion::{CompletionArgument, CompletionReference, CompletionResult};
use crate::features::prompts::PromptGetResult;
use crate::health::{self, ConnectionState, Snapshot, StartupDecision as Health};
use crate::mcp_contract::{ConfigSource, McpServerConfig, WorkspaceAdmission};
use crate::model_catalog::ServerSummary;
use crate::native_config::NativeConfigLoad;
use crate::project_config::{WorkspaceDiagnostic, render_workspace_diagnostic};
use crate::server_lifecycle::{Lifecycle, Server};
use crate::server_transport::ConnectOptions;
use crate::server_views::{health_failure, model_summary, snapshot_server};
use crate::startup_admission::{StartupDecision, StartupPhase, decide_startup};
use crate::timing::{sleep, spawn, spawn_on};
use crate::tool_mcp_feature_dispatch::NAME as FEATURES_TOOL;
use crate::tool_mcp_registry::{SchemaLimits, publish_tools};
use crate::tool_names::ToolNames;
use crate::tool_search::{self, Search, SearchLimits};
use crate::transport::ShutdownMode;

const REQUIRED_FALLBACK: &str = "Check the trusted profile configuration and retry.";
const STARTUP_POLL: Duration = Duration::from_millis(5);
const DISCOVERING: &str = r#"{"tools":[],"count":0,"state":"discovering","retryable":true}"#;
const SERVER_NOT_FOUND: &str = r#"{"tools":[],"count":0,"error":"McpServerNotFound"}"#;

pub struct McpRuntime {
    servers: Mutex<Vec<Arc<Server>>>,
    options: ConnectOptions,
    names: Mutex<ToolNames>,
    reserved: Vec<String>,
    limits: ContextLimits,
    catalog_generation: Arc<AtomicU64>,
    discovering: Arc<AtomicBool>,
    published: Mutex<Published>,
    workspace_diagnostics: Mutex<Vec<WorkspaceDiagnostic>>,
    installed: AtomicBool,
    reloading: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct Published {
    generation: Option<u64>,
    tools: Vec<Arc<dyn Tool>>,
    notices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadOutcome {
    Published {
        configured: usize,
        unavailable: Vec<String>,
        healthy: bool,
    },
    RetainedRequiredFailure(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReloadCancelled;

impl McpRuntime {
    pub fn new(
        load: NativeConfigLoad,
        options: ConnectOptions,
        mut reserved: Vec<String>,
        limits: ContextLimits,
    ) -> Self {
        reserved.push(FEATURES_TOOL.to_owned());
        let installed = hosts_anything(&load);
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
            servers: Mutex::new(servers),
            options,
            names: Mutex::new(ToolNames::default()),
            reserved,
            limits,
            catalog_generation,
            discovering: Arc::new(AtomicBool::new(false)),
            published: Mutex::new(Published::default()),
            workspace_diagnostics: Mutex::new(load.workspace_diagnostics),
            installed: AtomicBool::new(installed),
            reloading: tokio::sync::Mutex::new(()),
        }
    }

    pub(crate) fn installed(&self) -> bool {
        self.installed.load(Ordering::Acquire)
    }

    pub fn connect(&self, phase: StartupPhase) -> Settling {
        self.discovering.store(true, Ordering::Release);
        let mut settling = Settling::all(
            self.current()
                .into_iter()
                .filter(|server| decide_startup(&server.config, phase) == StartupDecision::Connect)
                .map(|server| (server, Step::Start)),
        );
        settling.discovery = Some(Discovery(Arc::clone(&self.discovering)));
        settling
    }

    fn early_answer(&self, request: &McpSearchRequest) -> Option<McpSearchResult> {
        let servers = self.current();
        if matches!(request.host, McpSearchHost::Ask { .. })
            && let Some(name) = request.server.as_deref()
            && !servers.iter().any(|known| known.config.name == name)
        {
            return Some(McpSearchResult::plain(SERVER_NOT_FOUND));
        }
        (self.discovering.load(Ordering::Acquire)
            && !servers.iter().any(|server| server.catalog().is_some()))
        .then(|| McpSearchResult::plain(DISCOVERING))
    }

    async fn refresh_tool_lists(&self, scope: Option<&str>) {
        for server in self.current() {
            if scope.is_some_and(|name| server.config.name != name) {
                continue;
            }
            let Lifecycle::Ready(client) = server.lifecycle() else {
                continue;
            };
            server
                .refresh_tools(&client, Instant::now() + client.operation_timeout)
                .await;
        }
    }

    pub(crate) fn search(&self, request: &McpSearchRequest) -> McpSearchResult {
        if let Some(answer) = self.early_answer(request) {
            return answer;
        }
        let mut search_result = self.limits.get(ContextLimitName::McpSearchResultBytes);
        if let McpSearchHost::Ask { result_bytes } = request.host
            && result_bytes < search_result.effective_bytes()
        {
            search_result.value = ContextLimitValue::Bytes(result_bytes);
        }
        tool_search::search(
            &self.current(),
            &self.names,
            &self.reserved,
            Search {
                query: &request.query,
                server: request.server.as_deref(),
            },
            SearchLimits {
                description: self.limits.get(ContextLimitName::McpDescriptionBytes),
                search_result,
                schema: SchemaLimits::from(&self.limits),
            },
        )
    }

    pub fn pending_workspace_names(&self) -> Vec<String> {
        self.current()
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
        let snapshot = self.snapshot_health();
        if health::startup_decision(&snapshot.servers) != Health::Blocked {
            return None;
        }
        let failure = snapshot
            .servers
            .iter()
            .find(|server| server.required && server.connection != ConnectionState::Ready)
            .map_or_else(
                || "A required MCP server is unavailable.".to_owned(),
                |server| {
                    format!(
                        "Required MCP server '{}' failed to start: {}",
                        server.configured_name,
                        server.failure.as_deref().unwrap_or(REQUIRED_FALLBACK)
                    )
                },
            );
        Some(failure)
    }

    pub fn workspace_diagnostics(&self) -> Vec<WorkspaceDiagnostic> {
        lock(&self.workspace_diagnostics).clone()
    }

    pub fn render_health(&self) -> String {
        health::render(&self.snapshot_health())
    }

    pub fn model_catalog(&self) -> Vec<ServerSummary> {
        self.current()
            .iter()
            .map(|server| model_summary(server))
            .collect()
    }

    pub fn render_summary(&self) -> String {
        health::render_summary(&self.snapshot_health())
    }

    pub fn startup_notice(&self) -> Option<String> {
        health::render_startup_notice(&self.snapshot_health())
    }

    pub fn shutdown(&self, mode: ShutdownMode) -> Settling {
        Settling::all(
            self.current()
                .into_iter()
                .map(|server| (server, Step::Stop(mode))),
        )
    }

    pub fn revoke_workspace_except(&self, names: &[String]) -> bool {
        let revoked: Vec<Arc<Server>> = {
            let mut servers = lock(&self.servers);
            let (revoked, kept) = std::mem::take(&mut *servers)
                .into_iter()
                .partition(|server| {
                    approved_workspace(&server.config) && !names.contains(&server.config.name)
                });
            *servers = kept;
            revoked
        };
        if revoked.is_empty() {
            return false;
        }
        for server in revoked {
            if let Some(client) = server.retire() {
                spawn(async move { client.shutdown(ShutdownMode::Immediate).await });
            }
        }
        self.catalog_generation.fetch_add(1, Ordering::AcqRel);
        true
    }

    pub async fn list_resources(
        &self,
        server_name: &str,
        include_templates: bool,
    ) -> Result<Vec<ResourceSummary>, McpError> {
        let (server, deadline) = self.feature_server(server_name).await?;
        server.list_resources(include_templates, deadline).await
    }

    pub async fn read_resource(
        &self,
        server_name: &str,
        uri: &str,
    ) -> Result<Arc<[ResourceContent]>, FeatureFailure> {
        let (server, deadline) = self.feature_server(server_name).await?;
        server.read_resource(uri, deadline).await
    }

    pub async fn list_prompts(&self, server_name: &str) -> Result<Vec<PromptSummary>, McpError> {
        let (server, deadline) = self.feature_server(server_name).await?;
        server.list_prompts(deadline).await
    }

    pub async fn get_prompt(
        &self,
        server_name: &str,
        name: &str,
        arguments_json: &str,
    ) -> Result<PromptGetResult, FeatureFailure> {
        let (server, deadline) = self.feature_server(server_name).await?;
        server.get_prompt(name, arguments_json, deadline).await
    }

    pub async fn complete_prompt_argument(
        &self,
        server_name: &str,
        prompt_name: &str,
        argument: CompletionArgument<'_>,
        context: &[CompletionArgument<'_>],
    ) -> Result<CompletionResult, McpError> {
        self.complete(
            server_name,
            CompletionReference::Prompt(prompt_name),
            argument,
            context,
        )
        .await
    }

    pub async fn complete_resource_template_argument(
        &self,
        server_name: &str,
        uri_template: &str,
        argument: CompletionArgument<'_>,
        context: &[CompletionArgument<'_>],
    ) -> Result<CompletionResult, McpError> {
        self.complete(
            server_name,
            CompletionReference::ResourceTemplate(uri_template),
            argument,
            context,
        )
        .await
    }

    async fn complete(
        &self,
        server_name: &str,
        reference: CompletionReference<'_>,
        argument: CompletionArgument<'_>,
        context: &[CompletionArgument<'_>],
    ) -> Result<CompletionResult, McpError> {
        let (server, deadline) = self.feature_server(server_name).await?;
        server
            .complete(reference, argument, context, deadline)
            .await
    }

    async fn feature_server(&self, server_name: &str) -> Result<(Arc<Server>, Instant), McpError> {
        let server = self
            .current()
            .into_iter()
            .find(|server| server.config.name == server_name)
            .ok_or(McpError::McpServerNotFound)?;
        let deadline =
            Instant::now() + Duration::from_millis(server.config.operation_timeout_ms.into());
        await_feature_server(&server, deadline).await?;
        Ok((server, deadline))
    }

    pub fn current_outcome(&self) -> ReloadOutcome {
        published_outcome(&self.snapshot_health())
    }

    pub async fn reconcile(
        &self,
        candidate: NativeConfigLoad,
        retain_on_required_failure: bool,
        refresh_catalogs: bool,
        cancel: &CancellationToken,
    ) -> Result<ReloadOutcome, ReloadCancelled> {
        let _reloading = self.reloading.lock().await;
        if cancel.is_cancelled() {
            return Err(ReloadCancelled);
        }
        let installs = hosts_anything(&candidate);
        let retained_authority: Vec<String> = candidate
            .configs
            .iter()
            .filter(|config| config.enabled && approved_workspace(config))
            .map(|config| config.name.clone())
            .collect();
        let authority_reduced = self.revoke_workspace_except(&retained_authority);
        let current = self.current();
        let mut desired = Vec::with_capacity(candidate.configs.len());
        let mut reused = Vec::new();
        let mut fresh = Vec::new();
        for config in candidate.configs {
            if let Some(existing) = current
                .iter()
                .find(|server| server.config == config && running(server))
            {
                desired.push(Arc::clone(existing));
                reused.push(Arc::clone(existing));
                continue;
            }
            let server = Arc::new(Server::new(
                config,
                self.options.clone(),
                Arc::clone(&self.catalog_generation),
            ));
            desired.push(Arc::clone(&server));
            fresh.push(server);
        }
        let mut unpublished = Unpublished {
            servers: &fresh,
            armed: true,
        };
        let starting = Settling::all(
            fresh
                .iter()
                .filter(|server| {
                    decide_startup(&server.config, StartupPhase::All) == StartupDecision::Connect
                })
                .map(|server| (Arc::clone(server), Step::Start)),
        );
        if cancel.run_until_cancelled(starting).await.is_none() {
            stop_all(&fresh).await;
            return Err(ReloadCancelled);
        }
        if retain_on_required_failure
            && !authority_reduced
            && let Some(failure) = required_failure(&desired)
        {
            stop_all(&fresh).await;
            return Ok(ReloadOutcome::RetainedRequiredFailure(failure));
        }
        if cancel.is_cancelled() {
            stop_all(&fresh).await;
            return Err(ReloadCancelled);
        }
        let previous = std::mem::replace(&mut *lock(&self.servers), desired.clone());
        unpublished.armed = false;
        *lock(&self.workspace_diagnostics) = candidate.workspace_diagnostics;
        if installs {
            self.installed.store(true, Ordering::Release);
        }
        self.catalog_generation.fetch_add(1, Ordering::AcqRel);
        let removed: Vec<Arc<Server>> = previous
            .into_iter()
            .filter(|server| !desired.iter().any(|kept| Arc::ptr_eq(kept, server)))
            .collect();
        stop_all(&removed).await;
        if refresh_catalogs
            && cancel
                .run_until_cancelled(self.refresh_catalogs(&reused))
                .await
                .is_none()
        {
            return Err(ReloadCancelled);
        }
        Ok(self.current_outcome())
    }

    async fn refresh_catalogs(&self, servers: &[Arc<Server>]) {
        for server in servers {
            let Lifecycle::Ready(client) = server.lifecycle() else {
                continue;
            };
            server.features.request_refresh();
            client.request_tool_refresh();
            server
                .refresh_tools(&client, Instant::now() + client.operation_timeout)
                .await;
        }
    }

    fn snapshot_health(&self) -> Snapshot {
        Snapshot {
            servers: self
                .current()
                .iter()
                .map(|server| snapshot_server(server))
                .collect(),
            configuration_issues: lock(&self.workspace_diagnostics)
                .iter()
                .map(render_workspace_diagnostic)
                .collect(),
        }
    }

    pub(crate) fn current(&self) -> Vec<Arc<Server>> {
        lock(&self.servers).clone()
    }
}

async fn await_feature_server(server: &Server, deadline: Instant) -> Result<(), McpError> {
    if server.config.source == ConfigSource::Workspace
        && server.config.workspace_admission != Some(WorkspaceAdmission::Approved)
    {
        return Err(McpError::McpWorkspaceApprovalRequired);
    }
    while matches!(server.lifecycle(), Lifecycle::Starting) {
        if Instant::now() >= deadline {
            return Err(McpError::McpRequestTimedOut);
        }
        sleep(STARTUP_POLL).await;
    }
    Ok(())
}

fn hosts_anything(load: &NativeConfigLoad) -> bool {
    !load.configs.is_empty() || !load.workspace_diagnostics.is_empty()
}

fn approved_workspace(config: &McpServerConfig) -> bool {
    config.source == ConfigSource::Workspace
        && config.workspace_admission == Some(WorkspaceAdmission::Approved)
}

fn running(server: &Server) -> bool {
    matches!(server.lifecycle(), Lifecycle::Ready(client) if client.is_running())
}

fn required_failure(servers: &[Arc<Server>]) -> Option<String> {
    servers
        .iter()
        .filter(|server| server.config.required)
        .map(|server| snapshot_server(server))
        .find(|snapshot| snapshot.connection != ConnectionState::Ready)
        .map(|snapshot| {
            let failure = health_failure(true, snapshot.connection, snapshot.failure);
            format!(
                "Required MCP server '{}' failed to start: {}",
                snapshot.configured_name,
                failure.as_deref().unwrap_or(REQUIRED_FALLBACK)
            )
        })
}

fn published_outcome(snapshot: &Snapshot) -> ReloadOutcome {
    ReloadOutcome::Published {
        configured: snapshot.servers.len(),
        unavailable: snapshot
            .servers
            .iter()
            .filter(|server| {
                server.connection != ConnectionState::Ready
                    && (server.connection != ConnectionState::Disabled || server.required)
            })
            .map(|server| server.configured_name.clone())
            .collect(),
        healthy: health::startup_decision(&snapshot.servers) == Health::Ready,
    }
}

struct Unpublished<'a> {
    servers: &'a [Arc<Server>],
    armed: bool,
}

impl Drop for Unpublished<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for server in self.servers {
            if let Some(client) = server.retire()
                && let Ok(runtime) = Handle::try_current()
            {
                spawn_on(&runtime, async move {
                    client.shutdown(ShutdownMode::Immediate).await;
                });
            }
        }
    }
}

async fn stop_all(servers: &[Arc<Server>]) {
    Settling::all(
        servers
            .iter()
            .map(|server| (Arc::clone(server), Step::Stop(ShutdownMode::Immediate))),
    )
    .await;
}

#[derive(Debug, Clone, Copy)]
enum Step {
    Start,
    Stop(ShutdownMode),
}

pub struct Settling {
    tasks: Vec<JoinHandle<()>>,
    discovery: Option<Discovery>,
}

struct Discovery(Arc<AtomicBool>);

impl Drop for Discovery {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Settling {
    fn all(steps: impl Iterator<Item = (Arc<Server>, Step)>) -> Self {
        Self {
            tasks: steps
                .map(|(server, step)| spawn(settle(server, step)))
                .collect(),
            discovery: None,
        }
    }

    pub async fn abandon(mut self) {
        let tasks = std::mem::take(&mut self.tasks);
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
    }
}

impl Future for Settling {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        while let Some(task) = self.tasks.last_mut() {
            if Pin::new(task).poll(context).is_pending() {
                return Poll::Pending;
            }
            self.tasks.pop();
        }
        self.discovery = None;
        Poll::Ready(())
    }
}

impl Drop for Settling {
    fn drop(&mut self) {
        for task in &self.tasks {
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
            let servers = self.current();
            let mut names = lock(&self.names);
            let (tools, notices) = publish_tools(
                &servers,
                &mut names,
                &self.reserved,
                SchemaLimits::from(&self.limits),
            );
            published.generation = Some(generation);
            published.tools = tools;
            published.notices.extend(notices);
        }
        published.tools.clone()
    }

    fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut lock(&self.published).notices)
    }
}

impl McpToolSearch for McpRuntime {
    fn search_tools(
        self: Arc<Self>,
        request: McpSearchRequest,
    ) -> BoxFuture<'static, McpSearchResult> {
        Box::pin(async move {
            if self.early_answer(&request).is_none() {
                self.refresh_tool_lists(request.server.as_deref()).await;
            }
            match tokio::task::spawn_blocking(move || self.search(&request)).await {
                Ok(result) => result,
                Err(error) => std::panic::resume_unwind(error.into_panic()),
            }
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ofx_config::ContextLimitName;
    use ofx_config::ContextLimits;
    use ofx_contract::{PathAccess, PreparedCall, ToolContext, ToolResultStatus};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::catalog_freshness::SnapshotMetadata;
    use crate::feature_catalog::Snapshot;
    use crate::features::common::ResourceData;
    use crate::features::prompts::{PromptContentKind, PromptRole};
    use crate::features::resources::{Details, Resource};
    use crate::mcp_contract::{ConfigScope, EnvVar, McpServerConfig};
    use crate::project_config::WorkspaceDiagnosticCause;

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
      echo list >> "$STATE/lists"
      if [ -f "$STATE/changed" ]; then
        reply "$id" '{"tools":[{"name":"alpha","description":"Rewritten","inputSchema":{"type":"object"}},{"name":"change","inputSchema":{"type":"object"}},{"name":"crash","inputSchema":{"type":"object"}},{"name":"beta","inputSchema":{"type":"object"}}]}'
      else
        reply "$id" '{"tools":[{"name":"alpha","description":"First","inputSchema":{"type":"object"}},{"name":"change","inputSchema":{"type":"object"}},{"name":"crash","inputSchema":{"type":"object"}}]}'
      fi ;;
    *'"name":"change"'*)
      touch "$STATE/changed"
      printf '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}\n'
      reply "$id" '{"content":[{"type":"text","text":"changed"}]}' ;;
    *'"name":"crash"'*) exit 7 ;;
    *'"method":"tools/call"'*)
      printf '%s\n' "$line" >> "$STATE/calls"
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

    fn limits() -> ContextLimits {
        ContextLimits::default()
    }

    fn runtime(configs: Vec<McpServerConfig>) -> Arc<McpRuntime> {
        Arc::new(McpRuntime::new(
            NativeConfigLoad {
                configs,
                ..NativeConfigLoad::default()
            },
            ConnectOptions::default(),
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

    fn prepare(runtime: &McpRuntime, name: &str, arguments: &str) -> Box<dyn PreparedCall> {
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
    }

    async fn execute(prepared: Box<dyn PreparedCall>) -> ofx_contract::ToolOutput {
        prepared
            .execute(ToolContext::new(
                ofx_contract::ToolCallId::new("call"),
                CancellationToken::new(),
                PathAccess::WorkspaceOnly,
            ))
            .await
    }

    async fn call(runtime: &McpRuntime, name: &str, arguments: &str) -> ofx_contract::ToolOutput {
        execute(prepare(runtime, name, arguments)).await
    }

    #[cfg(target_os = "linux")]
    fn process_ended(pid: i32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, fields)| fields.starts_with('Z'))
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn process_ended(pid: i32) -> bool {
        std::process::Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .output()
            .map_or(true, |output| {
                let state = String::from_utf8_lossy(&output.stdout);
                let state = state.trim();
                state.is_empty() || state.starts_with('Z')
            })
    }

    async fn ended_within(pid: i32, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if process_ended(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        process_ended(pid)
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
        assert_eq!(
            runtime.tools()[1].spec().description,
            "MCP tool\n\nServer instructions: Prefer alpha."
        );
        let Ok(prepared) = runtime.tools()[0].prepare("{}") else {
            panic!("arguments were refused");
        };
        assert_eq!(
            prepared.review_schema().as_deref(),
            Some(
                r#"{"type":"function","name":"mcp_fixture_alpha","description":"First\n\nServer instructions: Prefer alpha.","inputSchema":{"type":"object"}}"#
            )
        );
        let states: Vec<_> = runtime
            .snapshot_health()
            .servers
            .into_iter()
            .map(|server| (server.connection, server.counts.tools))
            .collect();
        assert_eq!(
            states,
            [
                (ConnectionState::Ready, Some(3)),
                (ConnectionState::Disabled, None)
            ]
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
    async fn tool_arguments_reach_the_server_as_written_unless_refused_before_the_call() {
        let state = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![config("fixture", SERVER, state.path())]);
        runtime.connect(StartupPhase::All).await;
        let arguments = r#"{"z":1e3,"id":12345678901234567890123,"big":1e400}"#;
        let output = call(&runtime, "mcp_fixture_alpha", arguments).await;
        assert_eq!(
            output.status,
            ToolResultStatus::Success,
            "{}",
            output.content
        );
        let tool = runtime
            .tools()
            .into_iter()
            .find(|tool| tool.spec().name == "mcp_fixture_alpha")
            .expect("an advertised tool");
        let deep = format!("{{\"x\":{}0{}}}", "[".repeat(64), "]".repeat(64));
        let deepest = format!("{{\"x\":{}0{}}}", "[".repeat(63), "]".repeat(63));
        let crowded = format!("{{\"x\":[{}]}}", vec!["0"; 4095].join(","));
        let oversized = format!("{{\"x\":\"{}\"}}", "a".repeat(1024 * 1024));
        for (arguments, error) in [
            (deep.as_str(), Some("InstanceLimitExceeded")),
            (deepest.as_str(), None),
            (crowded.as_str(), Some("InstanceLimitExceeded")),
            (oversized.as_str(), Some("InstanceLimitExceeded")),
            ("[]", Some("InvalidJson")),
            (r#"{"x":{"y":1,"y":2}}"#, Some("InvalidJson")),
        ] {
            let refusal = tool.prepare(arguments).err().map(|output| {
                assert_eq!(output.status, ToolResultStatus::Failure);
                output.content
            });
            assert_eq!(
                refusal,
                error.map(|error| format!(
                    "Invalid arguments for MCP tool mcp_fixture_alpha: {error}"
                )),
                "{}",
                &arguments[..arguments.len().min(80)]
            );
        }
        let calls = std::fs::read_to_string(state.path().join("calls")).unwrap();
        let sent: Vec<&str> = calls.lines().collect();
        assert_eq!(sent.len(), 1, "{calls}");
        assert!(
            sent[0].ends_with(&format!(
                "\"method\":\"tools/call\",\"params\":{{\"name\":\"alpha\",\"arguments\":{arguments}}}}}"
            )),
            "{calls}"
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
    async fn a_call_prepared_before_its_tool_changed_never_reaches_the_server() {
        let state = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![config("fixture", SERVER, state.path())]);
        runtime.connect(StartupPhase::All).await;
        let outdated = prepare(&runtime, "mcp_fixture_alpha", r#"{"n":1}"#);
        let unchanged = prepare(&runtime, "mcp_fixture_change", "{}");
        let before = runtime.generation();
        assert_eq!(
            call(&runtime, "mcp_fixture_change", "{}").await.status,
            ToolResultStatus::Success
        );
        for _ in 0..200 {
            if runtime.generation() > before && names(&runtime).len() == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let output = execute(outdated).await;
        assert_eq!(output.status, ToolResultStatus::Failure);
        assert_eq!(
            output.content,
            "MCP tool definition changed before execution. Its current schema is loaded; review it before issuing a new call."
        );
        let calls = std::fs::read_to_string(state.path().join("calls")).unwrap_or_default();
        assert!(!calls.contains(r#""n":1"#), "{calls}");
        assert_eq!(execute(unchanged).await.status, ToolResultStatus::Success);
        let current = prepare(&runtime, "mcp_fixture_alpha", r#"{"n":2}"#);
        assert_eq!(
            current.review_schema().as_deref(),
            Some(
                r#"{"type":"function","name":"mcp_fixture_alpha","description":"Rewritten\n\nServer instructions: Prefer alpha.","inputSchema":{"type":"object"}}"#
            )
        );
        assert_eq!(execute(current).await.status, ToolResultStatus::Success);
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
        let failed = &runtime.snapshot_health().servers[0];
        assert_eq!(failed.connection, ConnectionState::Failed);
        assert_eq!(failed.failure.as_deref(), Some("MCP restart limit reached"));
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
        assert_eq!(
            runtime.snapshot_health().servers[0].connection,
            ConnectionState::Disabled
        );
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
            ConnectOptions::default(),
            Vec::new(),
            {
                let mut limits = limits();
                limits.apply_command_line(&[ofx_config::ContextLimitOverride {
                    name: ContextLimitName::McpSelectedSchemaBytes,
                    value: ContextLimitValue::Bytes(16),
                }]);
                limits
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

    fn lines(path: &Path) -> usize {
        std::fs::read_to_string(path).map_or(0, |text| text.lines().count())
    }

    fn load(configs: Vec<McpServerConfig>) -> NativeConfigLoad {
        NativeConfigLoad {
            configs,
            ..NativeConfigLoad::default()
        }
    }

    fn approved(config: McpServerConfig) -> McpServerConfig {
        McpServerConfig {
            source: ConfigSource::Workspace,
            scope: ConfigScope::Workspace,
            workspace_admission: Some(WorkspaceAdmission::Approved),
            ..config
        }
    }

    #[tokio::test]
    async fn a_reload_keeps_unchanged_servers_refreshes_them_and_starts_new_ones() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let kept = config("kept", SERVER, first.path());
        let runtime = runtime(vec![kept.clone()]);
        runtime.connect(StartupPhase::All).await;
        let generation = runtime.generation();
        let outcome = runtime
            .reconcile(
                load(vec![kept, config("added", SERVER, second.path())]),
                true,
                true,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(
            outcome,
            Ok(ReloadOutcome::Published {
                configured: 2,
                unavailable: Vec::new(),
                healthy: true
            })
        );
        assert_eq!(lines(&first.path().join("pids")), 1);
        assert_eq!(lines(&first.path().join("lists")), 2);
        assert_eq!(lines(&second.path().join("pids")), 1);
        assert!(runtime.generation() > generation);
        assert!(names(&runtime).contains(&"mcp_added_alpha".to_owned()));
        assert!(names(&runtime).contains(&"mcp_kept_alpha".to_owned()));
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_reload_replaces_a_changed_server_and_drops_a_removed_one() {
        let state = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![
            config("one", SERVER, state.path()),
            config("gone", SERVER, other.path()),
        ]);
        runtime.connect(StartupPhase::All).await;
        let changed = McpServerConfig {
            required: true,
            ..config("one", SERVER, state.path())
        };
        let outcome = runtime
            .reconcile(load(vec![changed]), true, true, &CancellationToken::new())
            .await;
        assert!(matches!(
            outcome,
            Ok(ReloadOutcome::Published { configured: 1, .. })
        ));
        assert_eq!(lines(&state.path().join("pids")), 2);
        let snapshot = runtime.snapshot_health();
        assert_eq!(snapshot.servers.len(), 1);
        assert!(snapshot.servers[0].required);
        assert!(
            !names(&runtime)
                .iter()
                .any(|name| name.starts_with("mcp_gone"))
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_required_server_that_fails_keeps_the_current_servers() {
        let state = tempfile::tempdir().unwrap();
        let kept = config("kept", SERVER, state.path());
        let runtime = runtime(vec![kept.clone()]);
        runtime.connect(StartupPhase::All).await;
        let broken = McpServerConfig {
            required: true,
            restart_limit: 0,
            ..config("broken", "echo 'boom' >&2; exit 3", state.path())
        };
        let outcome = runtime
            .reconcile(
                load(vec![kept.clone(), broken.clone()]),
                true,
                true,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(
            outcome,
            Ok(ReloadOutcome::RetainedRequiredFailure(
                "Required MCP server 'broken' failed to start: MCP server exited with code 3 before completing startup: boom".to_owned()
            ))
        );
        assert_eq!(runtime.snapshot_health().servers.len(), 1);
        assert!(names(&runtime).contains(&"mcp_kept_alpha".to_owned()));
        let outcome = runtime
            .reconcile(
                load(vec![kept, broken]),
                false,
                false,
                &CancellationToken::new(),
            )
            .await;
        let Ok(ReloadOutcome::Published {
            configured,
            unavailable,
            healthy,
        }) = outcome
        else {
            panic!("expected the candidate to be published");
        };
        assert_eq!((configured, healthy), (2, false));
        assert_eq!(unavailable, ["broken"]);
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn revoked_workspace_servers_stop_at_once_and_leave_profile_servers() {
        let state = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![
            approved(config("project", SERVER, state.path())),
            config("profile", SERVER, other.path()),
        ]);
        runtime.connect(StartupPhase::All).await;
        assert!(!runtime.revoke_workspace_except(&["project".to_owned()]));
        assert!(runtime.revoke_workspace_except(&[]));
        let remaining: Vec<_> = runtime
            .snapshot_health()
            .servers
            .into_iter()
            .map(|server| server.configured_name)
            .collect();
        assert_eq!(remaining, ["profile"]);
        assert!(
            !names(&runtime)
                .iter()
                .any(|name| name.starts_with("mcp_project"))
        );
        assert_eq!(
            runtime.current_outcome(),
            ReloadOutcome::Published {
                configured: 1,
                unavailable: Vec::new(),
                healthy: true
            }
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_reload_that_drops_project_approval_publishes_even_with_a_required_failure() {
        let state = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![approved(config("project", SERVER, state.path()))]);
        runtime.connect(StartupPhase::All).await;
        let broken = McpServerConfig {
            required: true,
            restart_limit: 0,
            ..config("broken", "exit 3", state.path())
        };
        let outcome = runtime
            .reconcile(load(vec![broken]), true, true, &CancellationToken::new())
            .await;
        assert!(matches!(
            outcome,
            Ok(ReloadOutcome::Published { healthy: false, .. })
        ));
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn an_aborted_reload_stops_candidates_that_already_connected() {
        let fast = tempfile::tempdir().unwrap();
        let slow = tempfile::tempdir().unwrap();
        let stalled = r#"echo $$ >> "$STATE/pids"; exec sleep 30"#;
        let runtime = runtime(Vec::new());
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let runtime = Arc::clone(&runtime);
            let cancel = cancel.clone();
            let candidate = load(vec![
                config("fast", SERVER, fast.path()),
                config("slow", stalled, slow.path()),
            ]);
            async move { runtime.reconcile(candidate, true, true, &cancel).await }
        });
        for _ in 0..500 {
            if lines(&fast.path().join("lists")) > 0 && lines(&slow.path().join("pids")) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(lines(&fast.path().join("lists")) > 0);
        cancel.cancel();
        task.abort();
        assert!(task.await.is_err_and(|error| error.is_cancelled()));
        for state in [&fast, &slow] {
            let pids = std::fs::read_to_string(state.path().join("pids")).unwrap();
            let pid: i32 = pids.lines().next().unwrap().parse().unwrap();
            assert!(
                ended_within(pid, Duration::from_secs(5)).await,
                "{pid} is still running"
            );
        }
        assert!(runtime.snapshot_health().servers.is_empty());
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_cancelled_reload_changes_nothing() {
        let state = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![config("kept", SERVER, state.path())]);
        runtime.connect(StartupPhase::All).await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = runtime
            .reconcile(
                load(vec![config("added", SERVER, other.path())]),
                true,
                true,
                &cancel,
            )
            .await;
        assert_eq!(outcome, Err(ReloadCancelled));
        assert_eq!(lines(&other.path().join("pids")), 0);
        assert_eq!(runtime.snapshot_health().servers[0].configured_name, "kept");
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_runtime_is_installed_once_a_published_load_has_servers_or_project_errors() {
        let state = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let runtime = runtime(Vec::new());
        assert!(!runtime.installed());
        runtime
            .reconcile(load(Vec::new()), true, true, &cancel)
            .await
            .unwrap();
        assert!(!runtime.installed());
        let broken = McpServerConfig {
            required: true,
            restart_limit: 0,
            ..config("broken", "exit 3", state.path())
        };
        assert!(matches!(
            runtime
                .reconcile(load(vec![broken]), true, true, &cancel)
                .await,
            Ok(ReloadOutcome::RetainedRequiredFailure(_))
        ));
        assert!(!runtime.installed());
        runtime
            .reconcile(
                load(vec![config("one", SERVER, state.path())]),
                true,
                true,
                &cancel,
            )
            .await
            .unwrap();
        assert!(runtime.installed());
        runtime
            .reconcile(load(Vec::new()), true, true, &cancel)
            .await
            .unwrap();
        assert!(runtime.installed());
        let mut diagnosed = load(Vec::new());
        diagnosed.workspace_diagnostics = vec![WorkspaceDiagnostic::new(
            WorkspaceDiagnosticCause::InvalidJson,
        )];
        let reported = McpRuntime::new(diagnosed, ConnectOptions::default(), Vec::new(), limits());
        assert!(reported.installed());
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn health_reports_connected_servers_and_project_configuration_errors() {
        let state = tempfile::tempdir().unwrap();
        let runtime = runtime(Vec::new());
        assert_eq!(runtime.render_health(), "No MCP servers configured.\n");
        assert_eq!(
            runtime.render_summary(),
            "MCP: no servers configured. Use /mcp add <name> <command> [args...]."
        );
        let mut candidate = load(vec![config("fixture", SERVER, state.path())]);
        candidate.workspace_diagnostics = vec![WorkspaceDiagnostic::new(
            WorkspaceDiagnosticCause::InvalidJson,
        )];
        runtime
            .reconcile(candidate, true, true, &CancellationToken::new())
            .await
            .unwrap();
        let health = runtime.render_health();
        assert!(health.starts_with(
            "MCP health (1 server):\n  fixture source=profile scope=profile policy=optional transport=stdio state=ready auth=none status=ready\n    negotiated_name=fixture negotiated_version=1.0 protocol=2025-11-25\n    tools=3 resources=0 templates=0 prompts=0 cache=fresh subscription=active\n    retry_attempt=0 retry_in_ms=none discovery=completed\nProject MCP configuration errors:\n"
        ), "{health}");
        assert_eq!(
            runtime.render_summary(),
            "MCP: 1 server — 1 ready, 0 connecting, 0 needs auth, 0 failed. Project .mcp.json errors: 1. Use /mcp list for details."
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    const RESOURCE_SERVER: &str = r#"
echo $$ >> "$STATE/pids"
if [ -f "$STATE/stall" ]; then while IFS= read -r line; do :; done; exit 0; fi
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"resources\":{\"listChanged\":true}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"}}" ;;
    *'"method":"tools/list"'*) reply "$id" '{"tools":[]}' ;;
    *'"method":"resources/list"'*'"cursor":"page-2"'*)
      echo page-2 >> "$STATE/requests"
      reply "$id" '{"resources":[{"uri":"memory://a","name":"a","title":"Alpha"}]}' ;;
    *'"method":"resources/list"'*)
      echo resources >> "$STATE/requests"
      if [ -f "$STATE/broken" ]; then
        reply "$id" '{"resources":[{"uri":"memory://bad"}]}'
      elif [ -f "$STATE/ttl" ]; then
        reply "$id" '{"resources":[{"uri":"memory://t","name":"t"}],"ttlMs":1}'
      elif [ -f "$STATE/changed" ]; then
        reply "$id" '{"resources":[{"uri":"memory://c","name":"c"}]}'
      else
        reply "$id" '{"resources":[{"uri":"memory://b","name":"b"}],"nextCursor":"page-2"}'
      fi ;;
    *'"method":"resources/templates/list"'*)
      echo templates >> "$STATE/requests"
      if [ -f "$STATE/no-templates" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"Method not found"}}\n' "$id"
        continue
      fi
      reply "$id" '{"resourceTemplates":[{"uriTemplate":"memory://{id}","name":"by id"}]}'
      printf '{"jsonrpc":"2.0","method":"notifications/resources/list_changed"}\n'
      if [ -f "$STATE/exit" ]; then exit 0; fi
      touch "$STATE/broken" ;;
  esac
done
"#;

    fn requests(state: &Path) -> String {
        std::fs::read_to_string(state.join("requests")).unwrap_or_default()
    }

    fn summaries(listing: &[ResourceSummary]) -> Vec<(&str, &str)> {
        listing
            .iter()
            .map(|item| {
                (
                    item.identity.as_str(),
                    item.title.as_deref().unwrap_or(&item.name),
                )
            })
            .collect()
    }

    fn counts(runtime: &McpRuntime) -> (Option<usize>, Option<usize>, health::CacheFreshness) {
        let server = runtime.snapshot_health().servers.remove(0);
        (
            server.counts.resources,
            server.counts.resource_templates,
            server.cache_freshness,
        )
    }

    async fn until_resources_invalidated(runtime: &McpRuntime) {
        for _ in 0..500 {
            if let Lifecycle::Ready(client) = runtime.current()[0].lifecycle()
                && client.resources_invalidation.pending()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the resource list change never arrived");
    }

    #[tokio::test]
    async fn resource_catalogs_page_cache_and_refresh_after_list_changes() {
        let state = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![config("fixture", RESOURCE_SERVER, state.path())]);
        runtime.connect(StartupPhase::All).await;
        assert_eq!(
            counts(&runtime),
            (None, None, health::CacheFreshness::Fresh)
        );

        let listing = runtime.list_resources("fixture", false).await.unwrap();
        assert_eq!(
            summaries(&listing),
            [("memory://a", "Alpha"), ("memory://b", "b")]
        );
        assert_eq!(
            runtime.list_resources("fixture", false).await.unwrap(),
            listing
        );
        assert_eq!(requests(state.path()), "resources\npage-2\n");
        assert_eq!(
            counts(&runtime),
            (Some(2), None, health::CacheFreshness::Fresh)
        );

        let templates = runtime.list_resources("fixture", true).await.unwrap();
        assert_eq!(summaries(&templates), [("memory://{id}", "by id")]);
        assert_eq!(requests(state.path()), "resources\npage-2\ntemplates\n");
        assert_eq!(
            counts(&runtime),
            (Some(2), Some(1), health::CacheFreshness::Fresh)
        );

        until_resources_invalidated(&runtime).await;
        assert_eq!(
            counts(&runtime),
            (Some(2), Some(1), health::CacheFreshness::Fresh)
        );
        assert_eq!(
            runtime.list_resources("fixture", false).await.unwrap(),
            listing
        );
        assert_eq!(
            runtime.list_resources("fixture", false).await.unwrap(),
            listing
        );
        assert_eq!(
            requests(state.path()),
            "resources\npage-2\ntemplates\nresources\n"
        );
        let health = runtime.snapshot_health().servers.remove(0);
        assert_eq!(
            health.cache_freshness,
            health::CacheFreshness::FailedRefresh
        );
        assert_eq!(health.retry_attempt, 1);
        assert!(
            health.retry_in_ms.is_some_and(|delay| delay <= 100),
            "{health:?}"
        );

        std::fs::remove_file(state.path().join("broken")).unwrap();
        std::fs::write(state.path().join("changed"), "").unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let changed = runtime.list_resources("fixture", false).await.unwrap();
        assert_eq!(summaries(&changed), [("memory://c", "c")]);
        let health = runtime.snapshot_health().servers.remove(0);
        assert_eq!(
            (
                health.cache_freshness,
                health.retry_attempt,
                health.retry_in_ms
            ),
            (health::CacheFreshness::Fresh, 0, None)
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    async fn stopped_after_a_list_change(runtime: &McpRuntime) -> Arc<Server> {
        runtime.list_resources("fixture", true).await.unwrap();
        let server = Arc::clone(&runtime.current()[0]);
        let Lifecycle::Ready(client) = server.lifecycle() else {
            panic!("the server is not ready");
        };
        for _ in 0..500 {
            if !client.is_running() && client.resources_invalidation.pending() {
                return server;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the server never stopped after its list change");
    }

    fn exiting(state: &Path, restart_limit: u8) -> Arc<McpRuntime> {
        std::fs::write(state.join("exit"), "").unwrap();
        runtime(vec![McpServerConfig {
            restart_limit,
            ..config("fixture", RESOURCE_SERVER, state)
        }])
    }

    #[tokio::test]
    async fn a_restart_drops_the_resource_catalogs_of_the_stopped_process() {
        let state = tempfile::tempdir().unwrap();
        let runtime = exiting(state.path(), 1);
        runtime.connect(StartupPhase::All).await;
        let server = stopped_after_a_list_change(&runtime).await;
        std::fs::write(state.path().join("changed"), "").unwrap();
        assert_eq!(
            summaries(&runtime.list_resources("fixture", false).await.unwrap()),
            [("memory://c", "c")]
        );
        assert_eq!(server.restarts(), 1);
        assert_eq!(
            summaries(&runtime.list_resources("fixture", true).await.unwrap()),
            [("memory://{id}", "by id")]
        );
        assert_eq!(
            requests(state.path()),
            "resources\npage-2\ntemplates\nresources\ntemplates\n"
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_restart_for_a_listing_stays_within_the_operation_timeout() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(state.path().join("exit"), "").unwrap();
        let runtime = runtime(vec![McpServerConfig {
            restart_limit: 1,
            operation_timeout_ms: 500,
            startup_timeout_ms: 10_000,
            ..config("fixture", RESOURCE_SERVER, state.path())
        }]);
        runtime.connect(StartupPhase::All).await;
        let server = stopped_after_a_list_change(&runtime).await;
        std::fs::write(state.path().join("stall"), "").unwrap();
        let started = Instant::now();
        let listing = runtime.list_resources("fixture", false).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            summaries(&listing),
            [("memory://a", "Alpha"), ("memory://b", "b")]
        );
        let Lifecycle::Failed(failure) = server.lifecycle() else {
            panic!("the stalled restart did not fail the server");
        };
        assert!(
            failure.starts_with("MCP server did not complete startup within ")
                && !failure.contains("startup_timeout_ms"),
            "{failure}"
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_catalog_fetched_from_a_replaced_connection_is_never_published() {
        let state = tempfile::tempdir().unwrap();
        let runtime = exiting(state.path(), 1);
        runtime.connect(StartupPhase::All).await;
        let server = stopped_after_a_list_change(&runtime).await;
        let Lifecycle::Ready(stopped) = server.lifecycle() else {
            panic!("the server is not ready");
        };
        let listing = runtime.list_resources("fixture", false).await.unwrap();
        let outdated = Snapshot {
            items: Arc::from(vec![Resource {
                uri: "memory://stale".to_owned(),
                name: "stale".to_owned(),
                details: Details::default(),
            }]),
            metadata: SnapshotMetadata::fresh(u64::MAX),
        };
        assert!(!server.publish_catalog(&stopped, outdated));
        assert_eq!(
            runtime.list_resources("fixture", false).await.unwrap(),
            listing
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn concurrent_listings_restart_a_stopped_server_once() {
        let state = tempfile::tempdir().unwrap();
        let runtime = exiting(state.path(), 1);
        runtime.connect(StartupPhase::All).await;
        let server = stopped_after_a_list_change(&runtime).await;
        let (resources, templates) = tokio::join!(
            runtime.list_resources("fixture", false),
            runtime.list_resources("fixture", true)
        );
        assert_eq!(
            summaries(&resources.unwrap()),
            [("memory://a", "Alpha"), ("memory://b", "b")]
        );
        assert_eq!(summaries(&templates.unwrap()), [("memory://{id}", "by id")]);
        assert_eq!(server.restarts(), 1);
        assert!(matches!(server.lifecycle(), Lifecycle::Ready(_)));
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_server_past_its_restart_limit_keeps_serving_its_last_resource_catalogs() {
        let state = tempfile::tempdir().unwrap();
        let runtime = exiting(state.path(), 0);
        runtime.connect(StartupPhase::All).await;
        let server = stopped_after_a_list_change(&runtime).await;
        let cached = [("memory://a", "Alpha"), ("memory://b", "b")];
        assert_eq!(
            summaries(&runtime.list_resources("fixture", false).await.unwrap()),
            cached
        );
        assert!(matches!(server.lifecycle(), Lifecycle::Failed(_)));
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            summaries(&runtime.list_resources("fixture", false).await.unwrap()),
            cached
        );
        assert_eq!(
            summaries(&runtime.list_resources("fixture", true).await.unwrap()),
            [("memory://{id}", "by id")]
        );
        assert_eq!(requests(state.path()), "resources\npage-2\ntemplates\n");
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn expired_catalogs_refetch_and_protocol_errors_name_the_failure() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(state.path().join("ttl"), "").unwrap();
        std::fs::write(state.path().join("no-templates"), "").unwrap();
        let runtime = runtime(vec![config("fixture", RESOURCE_SERVER, state.path())]);
        runtime.connect(StartupPhase::All).await;
        assert_eq!(
            runtime.list_resources("fixture", true).await,
            Err(McpError::ProtocolFailure)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            summaries(&runtime.list_resources("fixture", false).await.unwrap()),
            [("memory://t", "t")]
        );
        assert_eq!(requests(state.path()), "resources\ntemplates\nresources\n");
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn resource_listing_names_missing_unsupported_and_unapproved_servers() {
        let state = tempfile::tempdir().unwrap();
        let mut pending = config("pending", RESOURCE_SERVER, state.path());
        pending.source = ConfigSource::Workspace;
        pending.scope = ConfigScope::Workspace;
        pending.workspace_admission = Some(WorkspaceAdmission::Pending);
        let disabled = McpServerConfig {
            enabled: false,
            ..config("off", RESOURCE_SERVER, state.path())
        };
        let runtime = runtime(vec![
            config("tools", SERVER, state.path()),
            pending,
            disabled,
        ]);
        runtime.connect(StartupPhase::All).await;
        for (server, expected) in [
            ("missing", McpError::McpServerNotFound),
            ("tools", McpError::McpResourcesUnsupported),
            ("pending", McpError::McpWorkspaceApprovalRequired),
            ("off", McpError::McpResourcesUnsupported),
        ] {
            assert_eq!(
                runtime.list_resources(server, false).await,
                Err(expected),
                "{server}"
            );
        }
        assert_eq!(requests(state.path()), "");
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    const PROMPT_SERVER: &str = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"prompts\":{\"listChanged\":true}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"}}" ;;
    *'"method":"tools/list"'*) reply "$id" '{"tools":[]}' ;;
    *'"method":"prompts/list"'*'"cursor":"page-2"'*)
      echo page-2 >> "$STATE/requests"
      reply "$id" '{"prompts":[{"name":"explain","title":"Explain","arguments":[{"name":"topic"}]}]}'
      if [ ! -f "$STATE/notified" ]; then
        touch "$STATE/notified"
        printf '{"jsonrpc":"2.0","method":"notifications/prompts/list_changed"}\n'
      fi ;;
    *'"method":"prompts/list"'*)
      echo prompts >> "$STATE/requests"
      if [ -f "$STATE/broken" ]; then
        reply "$id" '{"prompts":[{"name":"review","arguments":[{"name":"focus"},{"name":"focus"}]}]}'
      elif [ -f "$STATE/changed" ]; then
        reply "$id" '{"prompts":[{"name":"summarize","description":"Summarize a topic"}]}'
      else
        reply "$id" '{"prompts":[{"name":"review","description":"Review code","arguments":[{"name":"focus","required":true}]}],"nextCursor":"page-2"}'
      fi ;;
  esac
done
"#;

    fn prompt_names(listing: &[PromptSummary]) -> Vec<(&str, Vec<(&str, bool)>)> {
        listing
            .iter()
            .map(|prompt| {
                (
                    prompt.name.as_str(),
                    prompt
                        .arguments
                        .iter()
                        .map(|argument| (argument.name.as_str(), argument.required))
                        .collect(),
                )
            })
            .collect()
    }

    fn prompt_health(runtime: &McpRuntime) -> (Option<usize>, health::CacheFreshness) {
        let server = runtime.snapshot_health().servers.remove(0);
        (server.counts.prompts, server.cache_freshness)
    }

    async fn until_prompts_invalidated(runtime: &McpRuntime) {
        for _ in 0..500 {
            if let Lifecycle::Ready(client) = runtime.current()[0].lifecycle()
                && client.prompts_invalidation.pending()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the prompt list change never arrived");
    }

    #[tokio::test]
    async fn prompt_catalogs_page_cache_and_refresh_after_list_changes() {
        let state = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![config("fixture", PROMPT_SERVER, state.path())]);
        runtime.connect(StartupPhase::All).await;
        assert_eq!(
            prompt_health(&runtime),
            (None, health::CacheFreshness::Fresh)
        );

        let listing = runtime.list_prompts("fixture").await.unwrap();
        assert_eq!(
            prompt_names(&listing),
            [
                ("explain", vec![("topic", false)]),
                ("review", vec![("focus", true)])
            ]
        );
        assert_eq!(listing[0].title.as_deref(), Some("Explain"));
        assert_eq!(listing[1].description.as_deref(), Some("Review code"));
        assert_eq!(requests(state.path()), "prompts\npage-2\n");
        assert_eq!(
            prompt_health(&runtime),
            (Some(2), health::CacheFreshness::Fresh)
        );

        until_prompts_invalidated(&runtime).await;
        assert_eq!(
            prompt_health(&runtime),
            (Some(2), health::CacheFreshness::Fresh)
        );
        std::fs::write(state.path().join("broken"), "").unwrap();
        assert_eq!(runtime.list_prompts("fixture").await.unwrap(), listing);
        assert_eq!(runtime.list_prompts("fixture").await.unwrap(), listing);
        assert_eq!(requests(state.path()), "prompts\npage-2\nprompts\n");
        let health = runtime.snapshot_health().servers.remove(0);
        assert_eq!(
            (health.counts.prompts, health.cache_freshness),
            (Some(2), health::CacheFreshness::FailedRefresh)
        );
        assert!(
            health.retry_in_ms.is_some_and(|delay| delay <= 100),
            "{health:?}"
        );

        std::fs::remove_file(state.path().join("broken")).unwrap();
        std::fs::write(state.path().join("changed"), "").unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let changed = runtime.list_prompts("fixture").await.unwrap();
        assert_eq!(prompt_names(&changed), [("summarize", Vec::new())]);
        assert_eq!(changed[0].description.as_deref(), Some("Summarize a topic"));
        assert_eq!(runtime.list_prompts("fixture").await.unwrap(), changed);
        assert_eq!(
            requests(state.path()),
            "prompts\npage-2\nprompts\nprompts\n"
        );
        assert_eq!(
            prompt_health(&runtime),
            (Some(1), health::CacheFreshness::Fresh)
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn prompt_listing_names_missing_unsupported_and_invalid_catalogs() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(state.path().join("broken"), "").unwrap();
        let runtime = runtime(vec![
            config("prompts", PROMPT_SERVER, state.path()),
            config("tools", SERVER, state.path()),
        ]);
        runtime.connect(StartupPhase::All).await;
        for (server, expected) in [
            ("missing", McpError::McpServerNotFound),
            ("tools", McpError::McpPromptsUnsupported),
            ("prompts", McpError::DuplicateArgument),
        ] {
            assert_eq!(
                runtime.list_prompts(server).await,
                Err(expected),
                "{server}"
            );
        }
        assert_eq!(requests(state.path()), "prompts\n");
        assert_eq!(runtime.snapshot_health().servers[0].counts.prompts, None);
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    const READ_SERVER: &str = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"resources\":{\"listChanged\":true}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"}}" ;;
    *'"method":"tools/list"'*) reply "$id" '{"tools":[]}' ;;
    *'"method":"resources/list"'*)
      echo resources >> "$STATE/catalogs"
      if [ -f "$STATE/broken" ]; then
        reply "$id" '{"resources":[{"uri":"memory://bad"}]}'
      else
        reply "$id" '{"resources":[{"uri":"memory://notes","name":"notes"}]}'
      fi ;;
    *'"method":"resources/templates/list"'*)
      echo templates >> "$STATE/catalogs"
      if [ -f "$STATE/templates.json" ]; then
        reply "$id" "$(cat "$STATE/templates.json")"
      else
        reply "$id" '{"resourceTemplates":[{"uriTemplate":"memory://items/{id}","name":"item"}]}'
      fi ;;
    *'"method":"resources/read"'*)
      uri=$(printf '%s' "$line" | sed -n 's/.*"uri":"\([^"]*\)".*/\1/p')
      echo "read $uri" >> "$STATE/requests"
      if [ -f "$STATE/crash" ]; then exit 0; fi
      ttl=""
      if [ -f "$STATE/ttl" ]; then ttl=',"ttlMs":1'; fi
      text=$(cat "$STATE/version" 2>/dev/null || echo v1)
      case "$uri" in
        memory://items/denied)
          printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32002,"message":"denied","data":{"uri":"%s"}}}\n' "$id" "$uri" ;;
        memory://items/*) reply "$id" "{\"contents\":[{\"uri\":\"$uri\",\"text\":\"item\"}]}" ;;
        *) reply "$id" "{\"contents\":[{\"uri\":\"$uri\",\"mimeType\":\"text/plain\",\"text\":\"$text\"}]$ttl}" ;;
      esac
      if [ -f "$STATE/notify" ]; then
        rm "$STATE/notify"
        touch "$STATE/broken"
        printf '{"jsonrpc":"2.0","method":"notifications/resources/list_changed"}\n'
      fi ;;
  esac
done
"#;

    fn texts(contents: &[ResourceContent]) -> Vec<(&str, Option<&str>, &str)> {
        contents
            .iter()
            .map(|content| {
                let ResourceData::Text(text) = &content.data else {
                    panic!("not a text resource");
                };
                (
                    content.uri.as_str(),
                    content.mime_type.as_deref(),
                    text.as_str(),
                )
            })
            .collect()
    }

    async fn read_text(runtime: &McpRuntime, uri: &str) -> String {
        let contents = runtime.read_resource("fixture", uri).await.unwrap();
        texts(&contents)[0].2.to_owned()
    }

    #[tokio::test]
    async fn resource_reads_cache_match_templates_and_name_their_failures() {
        let state = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![
            config("fixture", READ_SERVER, state.path()),
            config("tools", SERVER, state.path()),
        ]);
        runtime.connect(StartupPhase::All).await;
        let notes = runtime
            .read_resource("fixture", "memory://notes")
            .await
            .unwrap();
        assert_eq!(
            texts(&notes),
            [("memory://notes", Some("text/plain"), "v1")]
        );
        let lists = || std::fs::read_to_string(state.path().join("catalogs")).unwrap();
        assert_eq!(lists(), "resources\n");
        assert_eq!(
            runtime
                .read_resource("fixture", "memory://notes")
                .await
                .unwrap(),
            notes
        );
        assert_eq!(read_text(&runtime, "memory://items/7").await, "item");
        assert_eq!(lists(), "resources\ntemplates\n");
        let failure = |error| Err(FeatureFailure::Error(error));
        assert_eq!(
            runtime.read_resource("fixture", "memory://other").await,
            failure(McpError::McpResourceNotFound)
        );
        assert_eq!(
            runtime.read_resource("fixture", "memory://items/a/b").await,
            failure(McpError::McpResourceNotFound)
        );
        assert_eq!(
            runtime
                .read_resource("fixture", "memory://items/denied")
                .await,
            Err(FeatureFailure::Diagnostic(
                r#"MCP protocol error -32002: denied; data={"uri":"memory://items/denied"}"#
                    .to_owned()
            ))
        );
        assert_eq!(
            runtime.read_resource("missing", "memory://notes").await,
            failure(McpError::McpServerNotFound)
        );
        assert_eq!(
            runtime.read_resource("tools", "memory://notes").await,
            failure(McpError::McpResourcesUnsupported)
        );
        assert_eq!(
            requests(state.path()),
            "read memory://notes\nread memory://items/7\nread memory://items/denied\n"
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn expired_reads_fall_back_to_the_last_contents_when_the_server_goes_away() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(state.path().join("ttl"), "").unwrap();
        let runtime = runtime(vec![McpServerConfig {
            restart_limit: 0,
            ..config("fixture", READ_SERVER, state.path())
        }]);
        runtime.connect(StartupPhase::All).await;
        assert_eq!(read_text(&runtime, "memory://notes").await, "v1");
        tokio::time::sleep(Duration::from_millis(20)).await;
        std::fs::write(state.path().join("crash"), "").unwrap();
        std::fs::write(state.path().join("version"), "v2").unwrap();
        assert_eq!(read_text(&runtime, "memory://notes").await, "v1");
        assert_eq!(read_text(&runtime, "memory://notes").await, "v1");
        assert!(matches!(
            runtime.current()[0].lifecycle(),
            Lifecycle::Failed(_)
        ));
        assert_eq!(
            runtime.read_resource("fixture", "memory://items/7").await,
            Err(FeatureFailure::Error(McpError::McpConnectionClosed))
        );
        assert_eq!(
            requests(state.path()),
            "read memory://notes\nread memory://notes\n"
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_restarted_server_reads_afresh() {
        let state = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![config("fixture", READ_SERVER, state.path())]);
        runtime.connect(StartupPhase::All).await;
        assert_eq!(read_text(&runtime, "memory://notes").await, "v1");
        std::fs::write(state.path().join("crash"), "").unwrap();
        assert_eq!(
            runtime.read_resource("fixture", "memory://items/1").await,
            Err(FeatureFailure::Error(McpError::McpConnectionClosed))
        );
        assert_eq!(read_text(&runtime, "memory://notes").await, "v1");
        std::fs::remove_file(state.path().join("crash")).unwrap();
        std::fs::write(state.path().join("version"), "v2").unwrap();
        assert_eq!(read_text(&runtime, "memory://items/2").await, "item");
        assert_eq!(runtime.current()[0].restarts(), 1);
        assert_eq!(read_text(&runtime, "memory://notes").await, "v2");
        assert_eq!(
            requests(state.path()),
            "read memory://notes\nread memory://items/1\nread memory://items/2\nread memory://notes\n"
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_list_change_whose_refresh_fails_refuses_reads_until_the_catalog_settles() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(state.path().join("notify"), "").unwrap();
        let runtime = runtime(vec![config("fixture", READ_SERVER, state.path())]);
        runtime.connect(StartupPhase::All).await;
        assert_eq!(read_text(&runtime, "memory://notes").await, "v1");
        until_resources_invalidated(&runtime).await;
        assert_eq!(
            runtime.read_resource("fixture", "memory://notes").await,
            Err(FeatureFailure::Error(McpError::McpFeatureCatalogChanged))
        );
        std::fs::remove_file(state.path().join("broken")).unwrap();
        std::fs::write(state.path().join("version"), "v2").unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(read_text(&runtime, "memory://notes").await, "v2");
        assert_eq!(
            requests(state.path()),
            "read memory://notes\nread memory://notes\n"
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn template_catalog_failures_surface_instead_of_not_found() {
        let state = tempfile::tempdir().unwrap();
        let templates = state.path().join("templates.json");
        std::fs::write(
            &templates,
            r#"{"resourceTemplates":[],"nextCursor":"again"}"#,
        )
        .unwrap();
        let runtime = runtime(vec![config("fixture", READ_SERVER, state.path())]);
        runtime.connect(StartupPhase::All).await;
        let failure = |error| Err(FeatureFailure::Error(error));
        assert_eq!(
            runtime.read_resource("fixture", "memory://missing").await,
            failure(McpError::DuplicateCursor)
        );
        let bounded = format!("memory://{{value}}{}b", "a".repeat(2047));
        std::fs::write(
            &templates,
            serde_json::json!({"resourceTemplates": [{"uriTemplate": bounded, "name": "bounded"}]})
                .to_string(),
        )
        .unwrap();
        assert_eq!(
            runtime
                .read_resource("fixture", &format!("memory://{}", "a".repeat(4096)))
                .await,
            failure(McpError::McpResourceTemplateMatchLimitExceeded)
        );
        assert_eq!(requests(state.path()), "");
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    const PROMPT_GET_SERVER: &str = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":{\"prompts\":{\"listChanged\":true}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"}}" ;;
    *'"method":"tools/list"'*) reply "$id" '{"tools":[]}' ;;
    *'"method":"prompts/list"'*)
      echo list >> "$STATE/requests"
      if [ -f "$STATE/broken" ]; then
        reply "$id" '{"prompts":[{"name":"review","arguments":[{"name":"focus"},{"name":"focus"}]}]}'
      else
        reply "$id" '{"prompts":[{"name":"review","arguments":[{"name":"focus","required":true},{"name":"depth"}]},{"name":"plain"}]}'
      fi ;;
    *'"method":"prompts/get"'*)
      params=$(printf '%s' "$line" | sed -n 's/.*"params":\(.*\)}$/\1/p')
      echo "get $params" >> "$STATE/requests"
      if [ -f "$STATE/crash" ]; then exit 0; fi
      text=$(cat "$STATE/version" 2>/dev/null || echo v1)
      case "$line" in
        *'"name":"plain"'*)
          printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32603,"message":"rejected","data":{"name":"plain"}}}\n' "$id" ;;
        *) reply "$id" "{\"description\":\"Review code\",\"messages\":[{\"role\":\"user\",\"content\":{\"type\":\"text\",\"text\":\"$text\"}},{\"role\":\"assistant\",\"content\":{\"type\":\"resource_link\",\"uri\":\"git://repo\",\"name\":\"repo\"}}]}" ;;
      esac
      if [ -f "$STATE/notify" ]; then
        rm "$STATE/notify"
        touch "$STATE/broken"
        printf '{"jsonrpc":"2.0","method":"notifications/prompts/list_changed"}\n'
      fi ;;
  esac
done
"#;

    async fn prompt_text(runtime: &McpRuntime, arguments: &str) -> String {
        let result = runtime
            .get_prompt("fixture", "review", arguments)
            .await
            .unwrap();
        result.messages[0].content_json.clone()
    }

    #[tokio::test]
    async fn prompt_gets_check_their_arguments_and_name_their_failures() {
        let state = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![
            config("fixture", PROMPT_GET_SERVER, state.path()),
            config("tools", SERVER, state.path()),
        ]);
        runtime.connect(StartupPhase::All).await;
        let result = runtime
            .get_prompt(
                "fixture",
                "review",
                r#" {"depth": "2", "focus": "security"} "#,
            )
            .await
            .unwrap();
        assert_eq!(result.description.as_deref(), Some("Review code"));
        let messages: Vec<_> = result
            .messages
            .iter()
            .map(|message| {
                (
                    message.role,
                    message.content_kind,
                    message.content_json.as_str(),
                )
            })
            .collect();
        assert_eq!(
            messages,
            [
                (
                    PromptRole::User,
                    PromptContentKind::Text,
                    r#"{"type":"text","text":"v1"}"#
                ),
                (
                    PromptRole::Assistant,
                    PromptContentKind::ResourceLink,
                    r#"{"type":"resource_link","uri":"git://repo","name":"repo"}"#
                ),
            ]
        );
        let failure = |error| Err(FeatureFailure::Error(error));
        for (server, name, arguments, expected) in [
            ("fixture", "missing", "{}", McpError::McpPromptNotFound),
            ("fixture", "review", "{}", McpError::InvalidArguments),
            (
                "fixture",
                "review",
                r#"{"focus":1}"#,
                McpError::InvalidArguments,
            ),
            (
                "fixture",
                "plain",
                r#"{"focus":"a"}"#,
                McpError::InvalidArguments,
            ),
            ("missing", "review", "{}", McpError::McpServerNotFound),
            ("tools", "review", "{}", McpError::McpPromptsUnsupported),
        ] {
            assert_eq!(
                runtime.get_prompt(server, name, arguments).await,
                failure(expected),
                "{server} {name} {arguments}"
            );
        }
        assert_eq!(
            runtime.get_prompt("fixture", "plain", "{}").await,
            Err(FeatureFailure::Diagnostic(
                r#"MCP protocol error -32603: rejected; data={"name":"plain"}"#.to_owned()
            ))
        );
        assert_eq!(
            requests(state.path()),
            "list\nget {\"name\":\"review\",\"arguments\":{\"depth\":\"2\",\"focus\":\"security\"}}\nget {\"name\":\"plain\",\"arguments\":{}}\n"
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_prompt_get_after_a_failed_list_refresh_waits_for_the_catalog_to_settle() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(state.path().join("notify"), "").unwrap();
        let runtime = runtime(vec![config("fixture", PROMPT_GET_SERVER, state.path())]);
        runtime.connect(StartupPhase::All).await;
        let focus = r#"{"focus":"a"}"#;
        assert_eq!(
            prompt_text(&runtime, focus).await,
            r#"{"type":"text","text":"v1"}"#
        );
        until_prompts_invalidated(&runtime).await;
        assert_eq!(
            runtime.get_prompt("fixture", "review", focus).await,
            Err(FeatureFailure::Error(McpError::McpFeatureCatalogChanged))
        );
        std::fs::remove_file(state.path().join("broken")).unwrap();
        std::fs::write(state.path().join("version"), "v2").unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            prompt_text(&runtime, focus).await,
            r#"{"type":"text","text":"v2"}"#
        );
        let get = "get {\"name\":\"review\",\"arguments\":{\"focus\":\"a\"}}\n";
        assert_eq!(
            requests(state.path()),
            format!("list\n{get}list\nlist\n{get}")
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_prompt_get_restarts_a_stopped_server_and_lists_its_prompts_again() {
        let state = tempfile::tempdir().unwrap();
        let runtime = runtime(vec![config("fixture", PROMPT_GET_SERVER, state.path())]);
        runtime.connect(StartupPhase::All).await;
        let focus = r#"{"focus":"a"}"#;
        assert_eq!(
            prompt_text(&runtime, focus).await,
            r#"{"type":"text","text":"v1"}"#
        );
        std::fs::write(state.path().join("crash"), "").unwrap();
        assert_eq!(
            runtime.get_prompt("fixture", "review", focus).await,
            Err(FeatureFailure::Error(McpError::McpConnectionClosed))
        );
        std::fs::remove_file(state.path().join("crash")).unwrap();
        std::fs::write(state.path().join("version"), "v2").unwrap();
        assert_eq!(
            prompt_text(&runtime, focus).await,
            r#"{"type":"text","text":"v2"}"#
        );
        assert_eq!(runtime.current()[0].restarts(), 1);
        let get = "get {\"name\":\"review\",\"arguments\":{\"focus\":\"a\"}}\n";
        assert_eq!(
            requests(state.path()),
            format!("list\n{get}{get}list\n{get}")
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    const COMPLETION_SERVER: &str = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      capabilities='{"prompts":{},"resources":{"listChanged":true},"completions":{}}'
      if [ -f "$STATE/resources-only" ]; then capabilities='{"resources":{},"completions":{}}'; fi
      reply "$id" "{\"protocolVersion\":\"$version\",\"capabilities\":$capabilities,\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1.0\"}}" ;;
    *'"method":"tools/list"'*) reply "$id" '{"tools":[]}' ;;
    *'"method":"prompts/list"'*)
      echo prompts >> "$STATE/requests"
      reply "$id" '{"prompts":[{"name":"review","arguments":[{"name":"tone"}]}]}' ;;
    *'"method":"resources/templates/list"'*)
      echo templates >> "$STATE/requests"
      if [ -f "$STATE/broken" ]; then
        reply "$id" '{"resourceTemplates":[{"uriTemplate":"a://{x}","name":"a"},{"uriTemplate":"a://{x}","name":"a"}]}'
      else
        reply "$id" '{"resourceTemplates":[{"uriTemplate":"custom://project/{path}","name":"project"}]}'
      fi ;;
    *'"method":"completion/complete"'*)
      params=$(printf '%s' "$line" | sed -n 's/.*"params":\(.*\)}$/\1/p')
      echo "complete $params" >> "$STATE/requests"
      if [ -f "$STATE/crash" ]; then exit 0; fi
      case "$line" in
        *'"value":"fail"'*)
          printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"rejected"}}\n' "$id" ;;
        *'"ref/prompt"'*) reply "$id" '{"completion":{"values":["balpha","bbeta"],"total":5,"hasMore":true}}' ;;
        *) reply "$id" '{"completion":{"values":["src/alpha"]}}' ;;
      esac
      if [ -f "$STATE/notify" ]; then
        rm "$STATE/notify"
        touch "$STATE/broken"
        printf '{"jsonrpc":"2.0","method":"notifications/resources/list_changed"}\n'
      fi ;;
  esac
done
"#;

    fn completion<'a>(name: &'a str, value: &'a str) -> CompletionArgument<'a> {
        CompletionArgument { name, value }
    }

    #[tokio::test]
    async fn completions_resolve_their_reference_and_name_their_failures() {
        let state = tempfile::tempdir().unwrap();
        let resources_only = tempfile::tempdir().unwrap();
        std::fs::write(resources_only.path().join("resources-only"), "").unwrap();
        let runtime = runtime(vec![
            config("fixture", COMPLETION_SERVER, state.path()),
            config("resources", COMPLETION_SERVER, resources_only.path()),
            config("tools", SERVER, resources_only.path()),
        ]);
        runtime.connect(StartupPhase::All).await;
        assert_eq!(
            runtime
                .complete_prompt_argument("fixture", "review", completion("tone", "b"), &[])
                .await,
            Ok(CompletionResult {
                values: vec!["balpha".to_owned(), "bbeta".to_owned()],
                total: Some(5),
                has_more: Some(true),
            })
        );
        assert_eq!(
            runtime
                .complete_resource_template_argument(
                    "fixture",
                    "custom://project/{path}",
                    completion("path", "src/"),
                    &[completion("branch", "main")],
                )
                .await,
            Ok(CompletionResult {
                values: vec!["src/alpha".to_owned()],
                total: None,
                has_more: None,
            })
        );
        let long = "x".repeat(4097);
        for (reference, argument, expected) in [
            (
                "missing",
                completion("tone", "b"),
                McpError::McpPromptNotFound,
            ),
            (
                "review",
                completion("tone", &long),
                McpError::InvalidArgument,
            ),
            (
                "review",
                completion("tone", "fail"),
                McpError::ProtocolFailure,
            ),
        ] {
            assert_eq!(
                runtime
                    .complete_prompt_argument("fixture", reference, argument, &[])
                    .await,
                Err(expected),
                "{reference}"
            );
        }
        assert_eq!(
            runtime
                .complete_resource_template_argument(
                    "fixture",
                    "custom://project/readme",
                    completion("path", "src/"),
                    &[],
                )
                .await,
            Err(McpError::McpResourceTemplateNotFound)
        );
        for (server, expected) in [
            ("missing", McpError::McpServerNotFound),
            ("tools", McpError::McpCompletionUnsupported),
            ("resources", McpError::McpPromptsUnsupported),
        ] {
            assert_eq!(
                runtime
                    .complete_prompt_argument(server, "review", completion("tone", "b"), &[])
                    .await,
                Err(expected),
                "{server}"
            );
        }
        assert_eq!(
            requests(state.path()),
            concat!(
                "prompts\n",
                "complete {\"ref\":{\"type\":\"ref/prompt\",\"name\":\"review\"},\"argument\":{\"name\":\"tone\",\"value\":\"b\"}}\n",
                "templates\n",
                "complete {\"ref\":{\"type\":\"ref/resource\",\"uri\":\"custom://project/{path}\"},\"argument\":{\"name\":\"path\",\"value\":\"src/\"},\"context\":{\"arguments\":{\"branch\":\"main\"}}}\n",
                "complete {\"ref\":{\"type\":\"ref/prompt\",\"name\":\"review\"},\"argument\":{\"name\":\"tone\",\"value\":\"fail\"}}\n",
            )
        );
        assert_eq!(requests(resources_only.path()), "");
        runtime.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn a_completion_waits_for_a_changed_catalog_and_restarts_a_stopped_server() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(state.path().join("notify"), "").unwrap();
        let runtime = runtime(vec![config("fixture", COMPLETION_SERVER, state.path())]);
        runtime.connect(StartupPhase::All).await;
        let template = "custom://project/{path}";
        let complete = || {
            runtime.complete_resource_template_argument(
                "fixture",
                template,
                completion("path", "src/"),
                &[],
            )
        };
        let completed = Ok(CompletionResult {
            values: vec!["src/alpha".to_owned()],
            total: None,
            has_more: None,
        });
        assert_eq!(complete().await, completed);
        until_resources_invalidated(&runtime).await;
        assert_eq!(complete().await, Err(McpError::McpFeatureCatalogChanged));
        std::fs::remove_file(state.path().join("broken")).unwrap();
        std::fs::write(state.path().join("crash"), "").unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(complete().await, Err(McpError::McpConnectionClosed));
        std::fs::remove_file(state.path().join("crash")).unwrap();
        assert_eq!(complete().await, completed);
        assert_eq!(runtime.current()[0].restarts(), 1);
        let request = "complete {\"ref\":{\"type\":\"ref/resource\",\"uri\":\"custom://project/{path}\"},\"argument\":{\"name\":\"path\",\"value\":\"src/\"}}\n";
        assert_eq!(
            requests(state.path()),
            format!("templates\n{request}templates\ntemplates\n{request}templates\n{request}")
        );
        runtime.shutdown(ShutdownMode::Immediate).await;
    }
}
