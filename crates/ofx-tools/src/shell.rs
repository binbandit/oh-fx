mod presentation;
mod request;
mod snapshot_format;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ofx_contract::{
    ApplicableTarget, BoxFuture, CallDescription, CommandRequest, Concurrency, PathAccess,
    PreparedCall, TargetKind, Tool, ToolActivity, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};
use ofx_exec::{
    Environment, ManagedExecutions, Snapshot, StartCaptured, configured_login_shell, environment,
};
use ofx_workspace::{PathError, path_inside, resolve_workspace_or_external_path};
use tokio_util::sync::CancellationToken;

use crate::filesystem::tool_spec;
use request::{Action, ShellRequest};
use snapshot_format::{
    command_result, format_snapshot, runtime_failure, snapshot_failed, stop_result_failed,
};

const TOOL_NAME: &str = "shell";
const DESCRIPTION: &str = "Run every command with shell.run. Fast commands complete in one call; commands still running after yield_time_ms return one owned session_id and remain available across turns. Use shell.interact with that exact session_id: omit chars to observe, or provide chars to send exact input and then observe. Use shell.stop only when termination is requested. output_delta is always terminal-safe; unsafe bytes are escaped while full_output_handle retains exact output, so do not run a separate command merely to test output safety or shell usability. Never detach with &, nohup, setsid, or double-forking.";
const INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"request":{"oneOf":[{"type":"object","properties":{"action":{"type":"string","enum":["run"]},"command":{"type":"string","maxLength":65536,"description":"Shell command to execute exactly once."},"cwd":{"type":"string","description":"Working directory; defaults to the workspace."},"profile":{"type":"string","enum":["clean","user"],"description":"Defaults to user; clean skips user startup files. Mutually exclusive with shell."},"yield_time_ms":{"type":"integer","minimum":0,"maximum":30000,"description":"Initial observation window. Defaults to 30000; use 0 to return the owned running handle immediately."},"timeout_ms":{"type":"integer","minimum":1,"description":"Set only when the user explicitly requests a finite deadline. Omit for commands intended to remain running, receive input, continue across turns, or be stopped later."}},"additionalProperties":false,"required":["action","command"]},{"type":"object","properties":{"action":{"type":"string","enum":["interact"]},"session_id":{"type":"string","description":"Owned execution handle returned by shell.run."},"yield_time_ms":{"type":"integer","minimum":0,"maximum":300000,"description":"Wait before yielding output. Empty observations wait 5000-300000 ms; shorter values are raised to 5000. Non-empty input is capped at 30000 ms and keeps shorter requested waits. Defaults to 5000. If the process remains running, interact with the same session_id again; never rerun it."}},"additionalProperties":false,"required":["action","session_id"]},{"type":"object","properties":{"action":{"type":"string","enum":["stop"]},"session_id":{"type":"string","description":"Owned execution handle returned by shell.run."},"force":{"type":"boolean","description":"Use immediate force termination when true. Defaults to false."}},"additionalProperties":false,"required":["action","session_id"]}]}},"additionalProperties":false,"required":["request"]}"#;
const DEFAULT_YIELD_TIME_MS: u32 = 30_000;
const MAX_YIELD_TIME_MS: u32 = 30_000;
const DEFAULT_WAIT_CEILING_MS: u32 = 5_000;
const MAX_WAIT_CEILING_MS: u32 = 300_000;
const MAX_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;
const PATH_OUTSIDE_WORKSPACE: &str = "PathOutsideWorkspace";
const PATH_WHITESPACE: [char; 4] = [' ', '\t', '\r', '\n'];
const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;
const UNAVAILABLE: &str = "unavailable";

pub struct Shell {
    spec: ToolSpec,
    context: Arc<ShellContext>,
}

struct ShellContext {
    workspace_root: PathBuf,
    executions: ManagedExecutions,
    command_timeout: Option<Duration>,
}

impl Shell {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        executions: ManagedExecutions,
        command_timeout: Option<Duration>,
    ) -> Self {
        Self {
            spec: tool_spec(TOOL_NAME, DESCRIPTION, INPUT_SCHEMA),
            context: Arc::new(ShellContext {
                workspace_root: workspace_root.into(),
                executions,
                command_timeout,
            }),
        }
    }
}

impl Tool for Shell {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        let arguments = request::unwrap_request(arguments);
        let title = presentation::title(
            &arguments,
            &self.context.workspace_root,
            &self.context.executions,
        );
        let validated = request::decode(&arguments)
            .map_err(ToolOutput::failure)
            .and_then(|request| self.context.validate(request));
        let effect = match &validated {
            Ok(call) => call.effect(),
            Err(_) => ToolEffect::None,
        };
        let request = validated.as_ref().ok().map(Validated::command_request);
        Ok(Box::new(ShellCall {
            description: CallDescription {
                title,
                activity: ToolActivity::Command,
                effect,
                concurrency: Concurrency::Serial,
            },
            validated,
            request,
            context: Arc::clone(&self.context),
        }))
    }

    fn history_arguments(&self, arguments: &str) -> Option<String> {
        Some(request::history_arguments(arguments)).filter(|history| history != arguments)
    }
}

enum Validated {
    Run {
        request: ShellRequest,
        cwd: PathBuf,
        environment: Option<Environment>,
    },
    UnresolvedCwd {
        request: ShellRequest,
        cwd: PathBuf,
        failure: String,
    },
    Interact(ShellRequest),
    Stop(ShellRequest),
}

impl Validated {
    fn command_request(&self) -> CommandRequest {
        match self {
            Self::Run { request, cwd, .. } | Self::UnresolvedCwd { request, cwd, .. } => {
                CommandRequest::Run {
                    command: request.command.clone().unwrap_or_default(),
                    cwd: cwd.clone(),
                    terminal: request.tty,
                }
            }
            Self::Interact(request) if request.has_input() => CommandRequest::SendInput,
            Self::Interact(_) => CommandRequest::Observe,
            Self::Stop(_) => CommandRequest::Stop,
        }
    }

    fn effect(&self) -> ToolEffect {
        match self {
            Self::Interact(request) if !request.has_input() => ToolEffect::ReadOnly,
            Self::Run { .. } | Self::UnresolvedCwd { .. } | Self::Interact(_) | Self::Stop(_) => {
                ToolEffect::Mutating
            }
        }
    }
}

impl ShellContext {
    fn validate(&self, request: ShellRequest) -> Result<Validated, ToolOutput> {
        if let Some(problem) = request.argument_problem() {
            return Err(ToolOutput::failure(problem));
        }
        match request.action {
            Action::Run => {
                let cwd = match self.resolve_cwd(request.cwd.as_deref()) {
                    Ok(cwd) => cwd,
                    Err(error) => {
                        let failure = format!("shell run cwd is invalid: {error}");
                        let requested = request.cwd.clone().unwrap_or_default();
                        if lexically_inside(&self.workspace_root, &requested) {
                            return Err(ToolOutput::failure(failure));
                        }
                        return Ok(Validated::UnresolvedCwd {
                            request,
                            cwd: PathBuf::from(requested.trim_matches(PATH_WHITESPACE)),
                            failure,
                        });
                    }
                };
                let environment = if request.tty {
                    None
                } else {
                    let configured = configured_login_shell();
                    Some(
                        environment(configured.as_deref(), request.profile).map_err(|error| {
                            ToolOutput::failure(format!("shell run profile is invalid: {error}"))
                        })?,
                    )
                };
                Ok(Validated::Run {
                    request,
                    cwd,
                    environment,
                })
            }
            Action::Interact => Ok(Validated::Interact(request)),
            Action::Stop => Ok(Validated::Stop(request)),
        }
    }

    fn resolve_cwd(&self, requested: Option<&str>) -> Result<PathBuf, PathError> {
        match requested {
            None | Some(".") => Ok(self.workspace_root.clone()),
            Some(requested) => resolve_workspace_or_external_path(&self.workspace_root, requested),
        }
    }

    async fn run(
        &self,
        request: ShellRequest,
        cwd: PathBuf,
        environment: Option<Environment>,
        path_access: PathAccess,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let cwd = match path_access {
            PathAccess::WorkspaceOrExternal => cwd,
            PathAccess::WorkspaceOnly => match self.resolve_cwd(request.cwd.as_deref()) {
                Ok(current) if path_inside(&self.workspace_root, &current) => current,
                _ => return ToolOutput::failure(runtime_failure(PATH_OUTSIDE_WORKSPACE)),
            },
        };
        let (Some(environment), Some(command)) = (environment, request.command) else {
            return ToolOutput::failure(runtime_failure(UNAVAILABLE));
        };
        let input = StartCaptured {
            command,
            cwd,
            environment,
            max_output_bytes: MAX_COMMAND_OUTPUT_BYTES,
            timeout: request
                .timeout_ms
                .map(Duration::from_millis)
                .or(self.command_timeout),
            yield_time: Duration::from_millis(request.yield_time_ms.into()),
        };
        match self.executions.start_captured(input, cancel).await {
            Ok(snapshot) => finish_command(&snapshot),
            Err(error) => ToolOutput::failure(runtime_failure(&error.to_string())),
        }
    }

    async fn interact(&self, request: ShellRequest, cancel: &CancellationToken) -> ToolOutput {
        let has_input = request.has_input();
        let session_id = request.session_id.unwrap_or_default();
        if let Some(retained) = self.executions.tombstone_snapshot(&session_id) {
            if has_input {
                return ToolOutput::failure(runtime_failure("ExecutionTerminal"));
            }
            return finish_command(&retained);
        }
        if has_input {
            return ToolOutput::failure(runtime_failure("InvalidBackend"));
        }
        let ceiling = effective_interact_yield_time(has_input, request.yield_time_ms);
        match self
            .executions
            .wait(&session_id, Duration::from_millis(ceiling.into()), cancel)
            .await
        {
            Ok(snapshot) => finish_command(&snapshot),
            Err(error) => ToolOutput::failure(runtime_failure(&error.to_string())),
        }
    }

    async fn stop(&self, request: ShellRequest) -> ToolOutput {
        let session_id = request.session_id.unwrap_or_default();
        let snapshot = match self.executions.tombstone_snapshot(&session_id) {
            Some(retained) => retained,
            None => match self.executions.stop(&session_id, request.force).await {
                Ok(snapshot) => snapshot,
                Err(error) => return ToolOutput::failure(runtime_failure(&error.to_string())),
            },
        };
        let body = format_snapshot(&snapshot, MAX_TOOL_RESULT_BYTES);
        let output = if stop_result_failed(snapshot.state) {
            ToolOutput::failure(body)
        } else {
            ToolOutput::success(body)
        };
        output.with_command_result(command_result(&snapshot))
    }
}

fn finish_command(snapshot: &Snapshot) -> ToolOutput {
    let body = format_snapshot(snapshot, MAX_TOOL_RESULT_BYTES);
    let output = if snapshot_failed(snapshot) {
        ToolOutput::failure(body)
    } else {
        ToolOutput::success(body)
    };
    output.with_command_result(command_result(snapshot))
}

fn effective_interact_yield_time(has_input: bool, requested_ms: u32) -> u32 {
    if has_input {
        requested_ms.min(MAX_YIELD_TIME_MS)
    } else {
        requested_ms.clamp(DEFAULT_WAIT_CEILING_MS, MAX_WAIT_CEILING_MS)
    }
}

struct ShellCall {
    description: CallDescription,
    validated: Result<Validated, ToolOutput>,
    request: Option<CommandRequest>,
    context: Arc<ShellContext>,
}

impl PreparedCall for ShellCall {
    fn describe(&self) -> CallDescription {
        self.description.clone()
    }

    fn command_request(&self) -> Option<&CommandRequest> {
        self.request.as_ref()
    }

    fn refusal(&self) -> Option<&ToolOutput> {
        self.validated.as_ref().err()
    }

    fn applicable_target(&self) -> Option<ApplicableTarget> {
        let Ok(Validated::Run { request, .. }) = &self.validated else {
            return None;
        };
        let cwd = self.context.resolve_cwd(request.cwd.as_deref()).ok()?;
        Some(ApplicableTarget {
            path: cwd,
            kind: TargetKind::Directory,
        })
    }

    fn execute(self: Box<Self>, context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        let ShellCall {
            validated,
            context: shell,
            ..
        } = *self;
        Box::pin(async move {
            let cancel = context.cancellation;
            match validated {
                Err(failure) => failure,
                Ok(Validated::Run {
                    request,
                    cwd,
                    environment,
                }) => {
                    shell
                        .run(request, cwd, environment, context.path_access, &cancel)
                        .await
                }
                Ok(Validated::UnresolvedCwd { failure, .. }) => ToolOutput::failure(failure),
                Ok(Validated::Interact(request)) => shell.interact(request, &cancel).await,
                Ok(Validated::Stop(request)) => shell.stop(request).await,
            }
        })
    }
}

fn lexically_inside(workspace_root: &Path, requested: &str) -> bool {
    let requested = requested.trim_matches(PATH_WHITESPACE);
    let path = Path::new(requested);
    if requested.starts_with('~')
        || path
            .components()
            .any(|component| component == Component::ParentDir)
    {
        return false;
    }
    path_inside(workspace_root, &workspace_root.join(path))
}

#[cfg(test)]
mod tests;
