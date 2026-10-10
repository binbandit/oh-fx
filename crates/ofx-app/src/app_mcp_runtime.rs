use std::env;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_config::{ProfilePaths, Settings};
use ofx_contract::{Notice, NoticeTone, UiEvent};
use ofx_mcp::{
    McpRuntime, NativeConfigLoad, ProfileStoreError, ProjectMcpChoices, ReloadOutcome,
    load_native_configs, preview_workspace_authority, profile_config_path,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::app_agent_runtime::Emit;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReloadFailure {
    Cancelled,
    AuthorityReduced,
    Failed,
}

struct Pending {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

pub(crate) struct McpHost {
    runtime: Arc<McpRuntime>,
    sources: McpSources,
    emit: Emit,
    pending: Mutex<Option<Pending>>,
}

impl McpHost {
    pub(crate) fn new(runtime: Arc<McpRuntime>, sources: McpSources, emit: Emit) -> Self {
        Self {
            runtime,
            sources,
            emit,
            pending: Mutex::new(None),
        }
    }

    pub(crate) fn runtime(&self) -> &McpRuntime {
        &self.runtime
    }

    pub(crate) fn sources(&self) -> &McpSources {
        &self.sources
    }

    pub(crate) fn begin_reload(&self) {
        let runtime = Arc::clone(&self.runtime);
        let sources = self.sources.clone();
        self.replace_pending(|cancel| async move { reload(&runtime, &sources, &cancel).await });
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
        self.cancel_pending();
        self.runtime.revoke_workspace_except(&[]);
        let runtime = Arc::clone(&self.runtime);
        let sources = self.sources.clone();
        self.replace_pending(move |cancel| async move {
            reduce(&runtime, &sources, rebuild, &cancel).await
        });
    }

    fn replace_pending<F, W>(&self, work: W)
    where
        W: FnOnce(CancellationToken) -> F,
        F: Future<Output = Result<ReloadOutcome, ReloadFailure>> + Send + 'static,
    {
        self.cancel_pending();
        let cancel = CancellationToken::new();
        let emit = Arc::clone(&self.emit);
        let reloading = work(cancel.clone());
        let task = tokio::spawn(async move {
            if let Some(notice) = completion_notice(reloading.await) {
                emit(UiEvent::Notice { notice });
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
}

impl Drop for McpHost {
    fn drop(&mut self) {
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
    let Ok(authority) = sources.authority() else {
        return Err(if runtime.revoke_workspace_except(&[]) {
            ReloadFailure::AuthorityReduced
        } else {
            ReloadFailure::Failed
        });
    };
    let reduced = runtime.revoke_workspace_except(&authority);
    let Ok(candidate) = sources.load() else {
        return Err(if reduced {
            ReloadFailure::AuthorityReduced
        } else {
            ReloadFailure::Failed
        });
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
    let candidate = sources.load().map_err(|_| ReloadFailure::Failed)?;
    runtime
        .reconcile(candidate, false, false, cancel)
        .await
        .map_err(|_| ReloadFailure::Cancelled)
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
        Err(ReloadFailure::Failed) => (NoticeTone::Warning, RELOAD_FAILED.to_owned()),
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
            body(Err(ReloadFailure::Failed)),
            Some((NoticeTone::Warning, RELOAD_FAILED.to_owned()))
        );
        assert_eq!(
            body(Err(ReloadFailure::AuthorityReduced)),
            Some((NoticeTone::Warning, AUTHORITY_REDUCED_FAILED.to_owned()))
        );
        assert_eq!(body(Err(ReloadFailure::Cancelled)), None);
    }
}
