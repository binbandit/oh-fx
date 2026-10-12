use std::env;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_config::{ProfilePaths, Settings};
use ofx_contract::{Notice, NoticeTone, UiEvent};
use ofx_mcp::{
    AuthenticationOutcome, McpError, McpRuntime, NativeConfigLoad, ProfileStoreError,
    ProjectMcpChoices, ReloadOutcome, load_native_configs, preview_workspace_authority,
    profile_config_path,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::app_agent_runtime::Emit;
use crate::mcp_auth_notices;
use crate::mcp_commands::{
    Completion, render_completions, render_prompt_get, render_prompt_listing,
    render_resource_listing, render_resource_read,
};

pub(crate) const TOPIC: &str = "mcp";
pub(crate) const RECONNECTING: &str = "MCP reconnection started. Your existing MCP servers will stay active while the new configuration is checked.";
const RELOADED: &str = "MCP configuration reloaded successfully.";
const RELOADED_EMPTY: &str = "MCP configuration reloaded. No servers are configured.";
const SOME_UNAVAILABLE: &str =
    "MCP configuration reloaded, but some servers are unavailable. Run /mcp list for details.";
const RELOAD_FAILED: &str = "MCP configuration could not be reloaded. Your existing MCP servers are still active. Check the configuration and run /mcp list for details before trying again.";
const AUTHORITY_REDUCED_FAILED: &str = "MCP configuration could not be reloaded after project authority was reduced. MCP is unavailable; check the configuration and run /mcp reload.";

#[derive(Debug, Clone)]
pub(crate) struct McpSources {
    paths: Option<ProfilePaths>,
    workspace_root: PathBuf,
}

impl McpSources {
    pub(crate) fn new(paths: Option<ProfilePaths>, workspace_root: PathBuf) -> Self {
        Self {
            paths,
            workspace_root,
        }
    }

    pub(crate) fn profile_path(&self) -> Option<PathBuf> {
        self.paths.as_ref().map(profile_config_path)
    }

    pub(crate) fn paths(&self) -> Option<&ProfilePaths> {
        self.paths.as_ref()
    }

    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub(crate) fn load_with(
        &self,
        settings: &Settings,
    ) -> Result<NativeConfigLoad, ProfileStoreError> {
        self.load_choices(project_choices(settings).as_ref())
    }

    fn load(&self) -> Result<NativeConfigLoad, ProfileStoreError> {
        self.load_choices(self.current_choices().as_ref())
    }

    fn load_choices(
        &self,
        choices: Option<&ProjectMcpChoices>,
    ) -> Result<NativeConfigLoad, ProfileStoreError> {
        load_native_configs(
            self.profile_path().as_deref(),
            &self.workspace_root,
            choices,
            &|name| env::var(name).ok(),
        )
    }

    fn authority(&self) -> Result<Vec<String>, ProfileStoreError> {
        preview_workspace_authority(
            &self.workspace_root,
            self.current_choices().as_ref(),
            &|name| env::var(name).ok(),
        )
    }

    fn current_choices(&self) -> Option<ProjectMcpChoices> {
        let Some(paths) = &self.paths else {
            return Some(ProjectMcpChoices::default());
        };
        let settings = Settings::load(paths, &self.workspace_root).ok()?;
        project_choices(&settings)
    }
}

fn project_choices(settings: &Settings) -> Option<ProjectMcpChoices> {
    ProjectMcpChoices::parse(settings.workspace_entry(), &mut Vec::new()).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReloadFailure {
    Cancelled,
    AuthorityReduced,
    Failed(String),
}

impl ReloadFailure {
    fn failed(error: ProfileStoreError) -> Self {
        Self::Failed(profile_error_name(error))
    }
}

fn profile_error_name(error: ProfileStoreError) -> String {
    match error {
        ProfileStoreError::Io(error) => McpError::from(error).to_string(),
        error => error.to_string(),
    }
}

type Settle = Box<dyn FnOnce(Result<ReloadOutcome, ReloadFailure>) -> Option<UiEvent> + Send>;

struct Pending {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

enum AuthenticationRefusal {
    Busy,
    Invalid(McpError),
}

struct PendingAuthentication {
    server: String,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

pub(crate) struct McpHost {
    runtime: Arc<McpRuntime>,
    sources: McpSources,
    emit: Emit,
    pending: Mutex<Option<Pending>>,
    authentication: Mutex<Option<PendingAuthentication>>,
}

impl McpHost {
    pub(crate) fn new(runtime: Arc<McpRuntime>, sources: McpSources, emit: Emit) -> Self {
        Self {
            runtime,
            sources,
            emit,
            pending: Mutex::new(None),
            authentication: Mutex::new(None),
        }
    }

    pub(crate) fn runtime(&self) -> &McpRuntime {
        &self.runtime
    }

    pub(crate) fn sources(&self) -> &McpSources {
        &self.sources
    }

    pub(crate) fn begin_reload(&self) {
        self.reload_with(Box::new(reload_notice));
    }

    fn reload_with(&self, settle: Settle) {
        self.discard_authentication("reload");
        let runtime = Arc::clone(&self.runtime);
        let sources = self.sources.clone();
        self.replace_pending(
            |cancel| async move { reload(&runtime, &sources, &cancel).await },
            settle,
        );
    }

    pub(crate) fn list_resources(&self, server: String, templates: bool) {
        let runtime = Arc::clone(&self.runtime);
        self.show_when_ready(async move {
            let listing = runtime.list_resources(&server, templates).await;
            render_resource_listing(&server, templates, listing)
        });
    }

    pub(crate) fn read_resource(&self, server: String, uri: String) {
        let runtime = Arc::clone(&self.runtime);
        self.show_when_ready(async move {
            let read = runtime.read_resource(&server, &uri).await;
            render_resource_read(&server, &uri, read)
        });
    }

    pub(crate) fn list_prompts(&self, server: String) {
        let runtime = Arc::clone(&self.runtime);
        self.show_when_ready(async move {
            let listing = runtime.list_prompts(&server).await;
            render_prompt_listing(&server, listing)
        });
    }

    pub(crate) fn get_prompt(&self, server: String, name: String, arguments: String) {
        let runtime = Arc::clone(&self.runtime);
        self.show_when_ready(async move {
            let result = runtime.get_prompt(&server, &name, &arguments).await;
            render_prompt_get(&server, &name, result)
        });
    }

    pub(crate) fn complete_prompt(&self, completion: Completion) {
        let runtime = Arc::clone(&self.runtime);
        self.show_when_ready(async move {
            let result = runtime
                .complete_prompt_argument(
                    &completion.server,
                    &completion.target,
                    completion.argument(),
                    &[],
                )
                .await;
            render_completions(&completion.server, "MCP prompt completion failed", result)
        });
    }

    pub(crate) fn complete_resource(&self, completion: Completion) {
        let runtime = Arc::clone(&self.runtime);
        self.show_when_ready(async move {
            let result = runtime
                .complete_resource_template_argument(
                    &completion.server,
                    &completion.target,
                    completion.argument(),
                    &[],
                )
                .await;
            render_completions(&completion.server, "MCP resource completion failed", result)
        });
    }

    fn show_when_ready(&self, body: impl Future<Output = String> + Send + 'static) {
        let emit = Arc::clone(&self.emit);
        tokio::spawn(async move {
            let body = body.await;
            emit(UiEvent::Notice {
                notice: Notice::new(NoticeTone::Neutral, TOPIC, body),
            });
        });
    }

    pub(crate) fn begin_authority_reduction(&self, rebuild: bool) {
        self.reduce_with(rebuild, Box::new(reload_notice));
    }

    fn reduce_with(&self, rebuild: bool, settle: Settle) {
        self.discard_authentication("authority_reduction");
        self.cancel_pending();
        self.runtime.revoke_workspace_except(&[]);
        let runtime = Arc::clone(&self.runtime);
        let sources = self.sources.clone();
        self.replace_pending(
            move |cancel| async move { reduce(&runtime, &sources, rebuild, &cancel).await },
            settle,
        );
    }

    fn replace_pending<F, W>(&self, work: W, settle: Settle)
    where
        W: FnOnce(CancellationToken) -> F,
        F: Future<Output = Result<ReloadOutcome, ReloadFailure>> + Send + 'static,
    {
        self.cancel_pending();
        let cancel = CancellationToken::new();
        let emit = Arc::clone(&self.emit);
        let reloading = work(cancel.clone());
        let task = tokio::spawn(async move {
            if let Some(event) = settle(reloading.await) {
                emit(event);
            }
        });
        *lock(&self.pending) = Some(Pending { cancel, task });
    }

    fn cancel_pending(&self) {
        if let Some(pending) = lock(&self.pending).take() {
            pending.cancel.cancel();
            pending.task.abort();
        }
    }

    pub(crate) fn begin_authentication(&self, server: &str) -> String {
        let profile = self.sources.profile_path();
        let started =
            self.start_authentication(server, move |name, result, reconnect| UiEvent::Notice {
                notice: mcp_auth_notices::completion(name, result, reconnect, profile.as_deref()),
            });
        match started {
            Ok(()) => mcp_auth_notices::waiting(server),
            Err(AuthenticationRefusal::Busy) => mcp_auth_notices::busy(server),
            Err(AuthenticationRefusal::Invalid(error)) => mcp_auth_notices::failed(server, &error),
        }
    }

    fn start_authentication<S>(&self, server: &str, settle: S) -> Result<(), AuthenticationRefusal>
    where
        S: FnOnce(&str, Result<AuthenticationOutcome, McpError>, Option<McpError>) -> UiEvent
            + Send
            + 'static,
    {
        let mut slot = lock(&self.authentication);
        let reloading = lock(&self.pending)
            .as_ref()
            .is_some_and(|pending| !pending.task.is_finished());
        if reloading
            || slot
                .as_ref()
                .is_some_and(|pending| !pending.task.is_finished())
        {
            return Err(AuthenticationRefusal::Busy);
        }
        self.runtime
            .validate_authentication_server(server)
            .map_err(AuthenticationRefusal::Invalid)?;
        let cancel = CancellationToken::new();
        let runtime = Arc::clone(&self.runtime);
        let emit = Arc::clone(&self.emit);
        let name = server.to_owned();
        let stop = cancel.clone();
        let task = tokio::spawn(async move {
            let open = |url: &str| ofx_auth::browser_allowed() && ofx_auth::open_url(url);
            let result = runtime.authenticate_server(&name, &open, &stop).await;
            let reconnect = match result {
                Ok(AuthenticationOutcome::Authenticated { .. }) => runtime
                    .reconnect_authenticated_server(&name, &stop)
                    .await
                    .err(),
                _ => None,
            };
            if stop.is_cancelled() {
                return;
            }
            emit(settle(&name, result, reconnect));
        });
        *slot = Some(PendingAuthentication {
            server: server.to_owned(),
            cancel,
            task,
        });
        Ok(())
    }

    pub(crate) fn logout(&self, server: String) -> Option<String> {
        if self.authentication_pending(&server) {
            return Some(mcp_auth_notices::logout_busy(&server));
        }
        let runtime = Arc::clone(&self.runtime);
        self.show_when_ready(async move {
            let result = runtime.logout_server(&server).await;
            mcp_auth_notices::logout_completion(&server, &result)
        });
        None
    }

    fn authentication_pending(&self, server: &str) -> bool {
        lock(&self.authentication)
            .as_ref()
            .is_some_and(|pending| pending.server == server && !pending.task.is_finished())
    }

    fn discard_authentication(&self, reason: &str) {
        let Some(pending) = lock(&self.authentication).take() else {
            return;
        };
        if !pending.task.is_finished() {
            ofx_trace::log(
                "mcp",
                format_args!(
                    "discarding pending authentication server={} reason={reason}",
                    pending.server
                ),
            );
        }
        pending.cancel.cancel();
        pending.task.abort();
    }
}

impl Drop for McpHost {
    fn drop(&mut self) {
        self.discard_authentication("shutdown");
        self.cancel_pending();
    }
}

async fn reload(
    runtime: &McpRuntime,
    sources: &McpSources,
    cancel: &CancellationToken,
) -> Result<ReloadOutcome, ReloadFailure> {
    if cancel.is_cancelled() {
        return Err(ReloadFailure::Cancelled);
    }
    let authority = match sources.authority() {
        Ok(authority) => authority,
        Err(error) => {
            return Err(if runtime.revoke_workspace_except(&[]) {
                ReloadFailure::AuthorityReduced
            } else {
                ReloadFailure::failed(error)
            });
        }
    };
    let reduced = runtime.revoke_workspace_except(&authority);
    let candidate = match sources.load() {
        Ok(candidate) => candidate,
        Err(error) => {
            return Err(if reduced {
                ReloadFailure::AuthorityReduced
            } else {
                ReloadFailure::failed(error)
            });
        }
    };
    runtime
        .reconcile(candidate, !reduced, true, cancel)
        .await
        .map_err(|_| ReloadFailure::Cancelled)
}

async fn reduce(
    runtime: &McpRuntime,
    sources: &McpSources,
    rebuild: bool,
    cancel: &CancellationToken,
) -> Result<ReloadOutcome, ReloadFailure> {
    if cancel.is_cancelled() {
        return Err(ReloadFailure::Cancelled);
    }
    if !rebuild {
        return Ok(runtime.current_outcome());
    }
    let candidate = sources.load().map_err(ReloadFailure::failed)?;
    runtime
        .reconcile(candidate, false, false, cancel)
        .await
        .map_err(|_| ReloadFailure::Cancelled)
}

fn reload_notice(result: Result<ReloadOutcome, ReloadFailure>) -> Option<UiEvent> {
    completion_notice(result).map(|notice| UiEvent::Notice { notice })
}

fn completion_notice(result: Result<ReloadOutcome, ReloadFailure>) -> Option<Notice> {
    let (tone, body) = match result {
        Ok(ReloadOutcome::Published {
            configured,
            unavailable,
            healthy,
        }) => {
            if healthy {
                let body = if configured == 0 {
                    RELOADED_EMPTY
                } else {
                    RELOADED
                };
                (NoticeTone::Neutral, body.to_owned())
            } else {
                (NoticeTone::Warning, unavailable_body(&unavailable))
            }
        }
        Ok(ReloadOutcome::RetainedRequiredFailure(failure)) => (
            NoticeTone::Warning,
            format!(
                "MCP configuration could not be reloaded. Your existing MCP servers are still active. {failure} Check the configuration or run /mcp list for details."
            ),
        ),
        Err(ReloadFailure::AuthorityReduced) => {
            (NoticeTone::Warning, AUTHORITY_REDUCED_FAILED.to_owned())
        }
        Err(ReloadFailure::Failed(_)) => (NoticeTone::Warning, RELOAD_FAILED.to_owned()),
        Err(ReloadFailure::Cancelled) => return None,
    };
    Some(Notice::new(tone, TOPIC, body))
}

fn unavailable_body(names: &[String]) -> String {
    match names {
        [] => SOME_UNAVAILABLE.to_owned(),
        [name] => format!(
            "MCP configuration reloaded, but server '{name}' is unavailable. Run /mcp list for details."
        ),
        names => {
            let quoted: Vec<String> = names.iter().map(|name| format!("'{name}'")).collect();
            format!(
                "MCP configuration reloaded, but {} servers are unavailable: {}. Run /mcp list for details.",
                names.len(),
                quoted.join(", ")
            )
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

mod menu;

#[cfg(test)]
mod tests {
    use super::*;

    fn body(result: Result<ReloadOutcome, ReloadFailure>) -> Option<(NoticeTone, String)> {
        completion_notice(result).map(|notice| {
            assert_eq!(notice.topic, TOPIC);
            (notice.tone, notice.body)
        })
    }

    fn published(configured: usize, unavailable: &[&str], healthy: bool) -> ReloadOutcome {
        ReloadOutcome::Published {
            configured,
            unavailable: unavailable.iter().map(|name| (*name).to_owned()).collect(),
            healthy,
        }
    }

    #[test]
    fn reload_completions_explain_healthy_and_degraded_results() {
        assert_eq!(
            body(Ok(published(2, &[], true))),
            Some((NoticeTone::Neutral, RELOADED.to_owned()))
        );
        assert_eq!(
            body(Ok(published(0, &[], true))),
            Some((NoticeTone::Neutral, RELOADED_EMPTY.to_owned()))
        );
        assert_eq!(
            body(Ok(published(2, &["alpha", "beta"], false))),
            Some((
                NoticeTone::Warning,
                "MCP configuration reloaded, but 2 servers are unavailable: 'alpha', 'beta'. Run /mcp list for details.".to_owned()
            ))
        );
        assert_eq!(
            body(Ok(published(1, &["alpha"], false))),
            Some((
                NoticeTone::Warning,
                "MCP configuration reloaded, but server 'alpha' is unavailable. Run /mcp list for details.".to_owned()
            ))
        );
        assert_eq!(
            body(Ok(published(1, &[], false))),
            Some((NoticeTone::Warning, SOME_UNAVAILABLE.to_owned()))
        );
    }

    #[test]
    fn reload_failures_say_whether_the_current_servers_stayed() {
        assert_eq!(
            body(Ok(ReloadOutcome::RetainedRequiredFailure(
                "Required MCP server 'db' failed to start: boom".to_owned()
            ))),
            Some((
                NoticeTone::Warning,
                "MCP configuration could not be reloaded. Your existing MCP servers are still active. Required MCP server 'db' failed to start: boom Check the configuration or run /mcp list for details.".to_owned()
            ))
        );
        assert_eq!(
            body(Err(ReloadFailure::Failed("LockBusy".to_owned()))),
            Some((NoticeTone::Warning, RELOAD_FAILED.to_owned()))
        );
        assert_eq!(
            body(Err(ReloadFailure::AuthorityReduced)),
            Some((NoticeTone::Warning, AUTHORITY_REDUCED_FAILED.to_owned()))
        );
        assert_eq!(body(Err(ReloadFailure::Cancelled)), None);
    }

    fn hanging_host() -> (
        McpHost,
        Arc<Mutex<Vec<UiEvent>>>,
        std::net::TcpListener,
        tempfile::TempDir,
    ) {
        use ofx_config::ContextLimits;
        use ofx_mcp::{ConnectOptions, McpServerConfig, TransportType};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let data = tempfile::tempdir().unwrap();
        let runtime = Arc::new(McpRuntime::new(
            NativeConfigLoad {
                configs: vec![McpServerConfig::remote("docs", TransportType::Http, &url)],
                ..NativeConfigLoad::default()
            },
            ConnectOptions {
                profile_data: Some(data.path().to_path_buf()),
                ..ConnectOptions::default()
            },
            Vec::new(),
            ContextLimits::default(),
        ));
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&events);
        let host = McpHost::new(
            runtime,
            McpSources::new(None, data.path().to_path_buf()),
            Arc::new(move |event| lock(&seen).push(event)),
        );
        (host, events, listener, data)
    }

    fn authentication_notices(events: &Mutex<Vec<UiEvent>>) -> usize {
        lock(events)
            .iter()
            .filter(|event| {
                matches!(event, UiEvent::Notice { notice } if notice.body.contains("authentication"))
            })
            .count()
    }

    #[tokio::test]
    async fn one_authentication_runs_at_a_time_and_a_reload_discards_it_silently() {
        let (host, events, listener, _data) = hanging_host();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            host.begin_authentication("docs"),
            mcp_auth_notices::waiting("docs")
        );
        let mut parked = None;
        for _ in 0..500 {
            if let Ok((stream, _)) = listener.accept() {
                parked = Some(stream);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(parked.is_some(), "the authentication reached discovery");
        assert_eq!(
            host.begin_authentication("docs"),
            mcp_auth_notices::busy("docs")
        );
        host.begin_reload();
        assert_eq!(
            host.begin_authentication("docs"),
            mcp_auth_notices::busy("docs")
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(authentication_notices(&events), 0);
        assert!(lock(&host.authentication).is_none());
    }

    #[tokio::test]
    async fn an_authentication_that_fails_reports_once() {
        let (host, events, listener, _data) = hanging_host();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                drop(stream);
            }
        });
        assert_eq!(
            host.begin_authentication("docs"),
            mcp_auth_notices::waiting("docs")
        );
        for _ in 0..200 {
            if authentication_notices(&events) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let notices: Vec<(NoticeTone, String)> = lock(&events)
            .iter()
            .filter_map(|event| match event {
                UiEvent::Notice { notice } => Some((notice.tone, notice.body.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            [(
                NoticeTone::Warning,
                "MCP authentication for 'docs' failed: ProtectedResourceMetadataUnavailable."
                    .to_owned()
            )]
        );
        assert_eq!(
            host.begin_authentication("absent"),
            "MCP authentication for 'absent' failed: McpServerNotFound."
        );
    }

    async fn mcp_notices(
        events: &Mutex<Vec<UiEvent>>,
        count: usize,
    ) -> Vec<(NoticeTone, String, String)> {
        let collect = || -> Vec<(NoticeTone, String, String)> {
            lock(events)
                .iter()
                .filter_map(|event| match event {
                    UiEvent::Notice { notice } if !notice.body.starts_with("MCP configuration") => {
                        Some((notice.tone, notice.topic.clone(), notice.body.clone()))
                    }
                    _ => None,
                })
                .collect()
        };
        for _ in 0..1000 {
            if collect().len() >= count {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        collect()
    }

    #[tokio::test]
    async fn a_logout_waits_only_for_a_pending_authentication_of_the_same_server() {
        let (host, events, _listener, _data) = hanging_host();
        assert_eq!(
            host.begin_authentication("docs"),
            mcp_auth_notices::waiting("docs")
        );
        assert_eq!(
            host.logout("docs".to_owned()),
            Some(mcp_auth_notices::logout_busy("docs"))
        );
        assert_eq!(host.logout("other".to_owned()), None);
        assert_eq!(
            mcp_notices(&events, 1).await,
            [(
                NoticeTone::Neutral,
                TOPIC.to_owned(),
                "MCP logout for 'other' failed: McpServerNotFound.".to_owned()
            )]
        );
        host.discard_authentication("reload");
        assert_eq!(host.logout("docs".to_owned()), None);
        assert_eq!(
            mcp_notices(&events, 2).await[1],
            (
                NoticeTone::Neutral,
                TOPIC.to_owned(),
                "No stored MCP credentials found for 'docs'.".to_owned()
            )
        );
    }

    #[tokio::test]
    async fn a_reload_leaves_a_logout_running() {
        let (host, events, _listener, _data) = hanging_host();
        assert_eq!(host.logout("docs".to_owned()), None);
        host.begin_reload();
        assert_eq!(
            mcp_notices(&events, 1).await,
            [(
                NoticeTone::Neutral,
                TOPIC.to_owned(),
                "No stored MCP credentials found for 'docs'.".to_owned()
            )]
        );
    }
}
