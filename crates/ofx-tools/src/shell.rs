mod presentation;
mod request;
mod snapshot_format;
mod terminal;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ofx_contract::{
    ApplicableTarget, BoxFuture, CallDescription, CommandProfile, CommandRequest, Concurrency,
    PathAccess, PreparedCall, TargetKind, Tool, ToolActivity, ToolContext, ToolEffect, ToolOutput,
    ToolSpec,
};
use ofx_exec::{
    Environment, HeldDirectory, ManagedExecutions, Profile, ShellSpec, Snapshot, StartCaptured,
    configured_login_shell, environment,
};
use ofx_workspace::{PathError, open_directory, path_inside, resolve_workspace_or_external_path};
use tokio_util::sync::CancellationToken;

use crate::filesystem::tool_spec;
use request::{Action, ShellRequest};
use snapshot_format::{
    command_result, format_snapshot, process_presentation, runtime_failure, snapshot_failed,
    stop_result_failed,
};
use terminal::terminal_shell;
pub use terminal::{ShellTerminals, TerminalOwner};

const TOOL_NAME: &str = "shell";
const DESCRIPTION: &str = "Run every command with shell.run. Fast commands complete in one call; commands still running after yield_time_ms return one owned session_id and remain available across turns. Use shell.interact with that exact session_id: omit chars to observe, or provide chars to send exact input and then observe. Use shell.stop only when termination is requested. output_delta is always terminal-safe; unsafe bytes are escaped while full_output_handle retains exact output, so do not run a separate command merely to test output safety or shell usability. Never detach with &, nohup, setsid, or double-forking. Each call starts a new shell with the user's startup files applied: their aliases, functions, and PATH are available, but cd, export, and alias changes do not carry over to the next call. In zsh, quote glob patterns meant for another program (for example '--include=*.zig') because unmatched globs are errors, and quote words that begin with =.";
const PROCESS_SCHEMA: &str = r#"{"type":"object","properties":{"request":{"oneOf":[{"type":"object","properties":{"action":{"type":"string","enum":["run"]},"command":{"type":"string","maxLength":65536,"description":"Shell command to execute exactly once."},"cwd":{"type":"string","description":"Working directory; defaults to the workspace."},"profile":{"type":"string","enum":["clean","user"],"description":"Defaults to user; clean skips user startup files. Mutually exclusive with shell."},"yield_time_ms":{"type":"integer","minimum":0,"maximum":30000,"description":"Initial observation window. Defaults to 30000; use 0 to return the owned running handle immediately."},"timeout_ms":{"type":"integer","minimum":1,"description":"Set only when the user explicitly requests a finite deadline. Omit for commands intended to remain running, receive input, continue across turns, or be stopped later."}},"additionalProperties":false,"required":["action","command"]},{"type":"object","properties":{"action":{"type":"string","enum":["interact"]},"session_id":{"type":"string","description":"Owned execution handle returned by shell.run."},"yield_time_ms":{"type":"integer","minimum":0,"maximum":300000,"description":"Wait before yielding output. Empty observations wait 5000-300000 ms; shorter values are raised to 5000. Non-empty input is capped at 30000 ms and keeps shorter requested waits. Defaults to 5000. If the process remains running, interact with the same session_id again; never rerun it."}},"additionalProperties":false,"required":["action","session_id"]},{"type":"object","properties":{"action":{"type":"string","enum":["stop"]},"session_id":{"type":"string","description":"Owned execution handle returned by shell.run."},"force":{"type":"boolean","description":"Use immediate force termination when true. Defaults to false."}},"additionalProperties":false,"required":["action","session_id"]}]}},"additionalProperties":false,"required":["request"]}"#;
const TERMINAL_SCHEMA: &str = r#"{"type":"object","properties":{"request":{"oneOf":[{"type":"object","properties":{"action":{"type":"string","enum":["run"]},"command":{"type":"string","maxLength":65536,"description":"Shell command to execute exactly once."},"cwd":{"type":"string","description":"Working directory; defaults to the workspace."},"profile":{"type":"string","enum":["clean","user"],"description":"Defaults to user; clean skips user startup files. Mutually exclusive with shell."},"tty":{"type":"boolean","description":"Use a persistent TTY when interactive input or human attachment is required. Defaults to false."},"yield_time_ms":{"type":"integer","minimum":0,"maximum":30000,"description":"Initial observation window. Defaults to 30000; use 0 to return the owned running handle immediately."},"timeout_ms":{"type":"integer","minimum":1,"description":"Set only when the user explicitly requests a finite deadline. Omit for commands intended to remain running, receive input, continue across turns, or be stopped later."},"reload":{"type":"boolean","description":"Reload the user's startup files before this command, for example after installing a tool or editing a file they source. Edits to the startup files themselves are picked up automatically."}},"additionalProperties":false,"required":["action","command"]},{"type":"object","properties":{"action":{"type":"string","enum":["run"]},"command":{"type":"string","maxLength":65536,"description":"Shell command to execute exactly once."},"cwd":{"type":"string","description":"Working directory; defaults to the workspace."},"shell":{"type":"object","properties":{"kind":{"type":"string","enum":["executable"]},"path":{"type":"string","description":"Absolute path to Bash or zsh."},"clean_start":{"type":"boolean","description":"Skip startup files when true."}},"additionalProperties":false,"required":["kind","path"]},"tty":{"type":"boolean","description":"Use a persistent TTY when interactive input or human attachment is required. Defaults to false."},"yield_time_ms":{"type":"integer","minimum":0,"maximum":30000,"description":"Initial observation window. Defaults to 30000; use 0 to return the owned running handle immediately."},"timeout_ms":{"type":"integer","minimum":1,"description":"Set only when the user explicitly requests a finite deadline. Omit for commands intended to remain running, receive input, continue across turns, or be stopped later."}},"additionalProperties":false,"required":["action","command","shell","tty"]},{"type":"object","properties":{"action":{"type":"string","enum":["interact"]},"session_id":{"type":"string","description":"Owned execution handle returned by shell.run."},"chars":{"type":"string","maxLength":65536,"description":"Exact characters to send to tty=true work before observing it. Omit or send an empty string to only observe. Observe application readiness before sending control characters. Use \\n for Enter and JSON escapes such as \\u0003 for control characters."},"yield_time_ms":{"type":"integer","minimum":0,"maximum":300000,"description":"Wait before yielding output. Empty observations wait 5000-300000 ms; shorter values are raised to 5000. Non-empty input is capped at 30000 ms and keeps shorter requested waits. Defaults to 5000. If the process remains running, interact with the same session_id again; never rerun it."}},"additionalProperties":false,"required":["action","session_id"]},{"type":"object","properties":{"action":{"type":"string","enum":["stop"]},"session_id":{"type":"string","description":"Owned execution handle returned by shell.run."},"force":{"type":"boolean","description":"Use immediate force termination when true. Defaults to false."}},"additionalProperties":false,"required":["action","session_id"]}]}},"additionalProperties":false,"required":["request"]}"#;
const DEFAULT_YIELD_TIME_MS: u32 = 30_000;
const MAX_YIELD_TIME_MS: u32 = 30_000;
const DEFAULT_WAIT_CEILING_MS: u32 = 5_000;
const MAX_WAIT_CEILING_MS: u32 = 300_000;
const MAX_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;
const PATH_OUTSIDE_WORKSPACE: &str = "PathOutsideWorkspace";
const DIRECTORY_CHANGED: &str = "CommandAuthorityContextMismatch";
const PATH_WHITESPACE: [char; 4] = [' ', '\t', '\r', '\n'];
const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;
const UNAVAILABLE: &str = "unavailable";

pub struct Shell {
    spec: ToolSpec,
    terminal_spec: Option<ToolSpec>,
    context: Arc<ShellContext>,
}

struct ShellContext {
    workspace_root: PathBuf,
    executions: ManagedExecutions,
    command_timeout: Option<Duration>,
    terminals: Option<ShellTerminals>,
}

impl Shell {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        executions: ManagedExecutions,
        command_timeout: Option<Duration>,
    ) -> Self {
        Self::build(workspace_root.into(), executions, command_timeout, None)
    }

    pub fn with_terminals(
        workspace_root: impl Into<PathBuf>,
        executions: ManagedExecutions,
        command_timeout: Option<Duration>,
        terminals: ShellTerminals,
    ) -> Self {
        Self::build(
            workspace_root.into(),
            executions,
            command_timeout,
            Some(terminals),
        )
    }

    fn build(
        workspace_root: PathBuf,
        executions: ManagedExecutions,
        command_timeout: Option<Duration>,
        terminals: Option<ShellTerminals>,
    ) -> Self {
        Self {
            spec: tool_spec(TOOL_NAME, DESCRIPTION, PROCESS_SCHEMA),
            terminal_spec: terminals
                .is_some()
                .then(|| tool_spec(TOOL_NAME, DESCRIPTION, TERMINAL_SCHEMA)),
            context: Arc::new(ShellContext {
                workspace_root,
                executions,
                command_timeout,
                terminals,
            }),
        }
    }
}

impl Tool for Shell {
    fn spec(&self) -> &ToolSpec {
        match (&self.terminal_spec, &self.context.terminals) {
            (Some(terminal_spec), Some(terminals)) if terminals.offered() => terminal_spec,
            _ => &self.spec,
        }
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        let arguments = request::unwrap_request(arguments);
        let presented = presentation::presentation(
            &arguments,
            &self.context.workspace_root,
            &self.context.executions,
        );
        let supports_tty = self
            .context
            .terminals
            .as_ref()
            .is_some_and(ShellTerminals::supported);
        let validated = request::decode(&arguments, supports_tty)
            .map_err(ToolOutput::failure)
            .and_then(|request| self.context.validate(request));
        let effect = match &validated {
            Ok(call) => call.effect(),
            Err(_) => ToolEffect::None,
        };
        let request = validated.as_ref().ok().map(Validated::command_request);
        Ok(Box::new(ShellCall {
            description: CallDescription {
                title: presented.title,
                label: presented.label,
                activity: ToolActivity::Command,
                effect,
                concurrency: Concurrency::Serial,
            },
            validated,
            request,
            context: Arc::clone(&self.context),
        }))
    }

    fn saved_arguments(&self, arguments: &str) -> Option<String> {
        request::saved_arguments(arguments)
    }

    fn request_arguments(&self, arguments: &str) -> Option<String> {
        request::request_arguments(arguments)
    }

    fn describe_saved(&self, arguments: &str) -> Option<CallDescription> {
        let arguments = request::unwrap_request(arguments);
        let presented = presentation::presentation(
            &arguments,
            &self.context.workspace_root,
            &self.context.executions,
        );
        Some(CallDescription {
            title: presented.title,
            label: presented.label,
            activity: ToolActivity::Command,
            effect: ToolEffect::None,
            concurrency: Concurrency::Serial,
        })
    }
}

enum Validated {
    Run {
        request: ShellRequest,
        cwd: WorkingDirectory,
        mode: RunMode,
    },
    UnresolvedCwd {
        request: ShellRequest,
        cwd: PathBuf,
        failure: String,
    },
    Interact(ShellRequest),
    Stop(ShellRequest),
}

enum RunMode {
    Captured(Environment),
    Terminal(Result<ShellSpec, String>),
}

impl RunMode {
    fn environment(&self) -> Option<&Environment> {
        match self {
            Self::Captured(environment) => Some(environment),
            Self::Terminal(_) => None,
        }
    }
}

impl Validated {
    fn command_request(&self) -> CommandRequest {
        match self {
            Self::Run { request, cwd, mode } => run_request(request, &cwd.path, mode.environment()),
            Self::UnresolvedCwd { request, cwd, .. } => run_request(request, cwd, None),
            Self::Interact(request) if request.has_input() => CommandRequest::SendInput {
                input: request.chars.clone().unwrap_or_default(),
            },
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

struct WorkingDirectory {
    path: PathBuf,
    held: Option<Result<HeldDirectory, PathError>>,
}

impl WorkingDirectory {
    fn hold(&mut self) {
        self.held = Some(hold_directory(&self.path));
    }

    fn still_named(self) -> Result<(PathBuf, HeldDirectory), String> {
        match self.held {
            Some(Err(error)) => Err(error.to_string()),
            Some(Ok(held))
                if hold_directory(&self.path).is_ok_and(|named| held.is_same_directory(&named)) =>
            {
                Ok((self.path, held))
            }
            Some(Ok(_)) | None => Err(DIRECTORY_CHANGED.to_owned()),
        }
    }
}

fn hold_directory(path: &Path) -> Result<HeldDirectory, PathError> {
    open_directory(path).map(HeldDirectory::new)
}

fn run_request(
    request: &ShellRequest,
    cwd: &Path,
    environment: Option<&Environment>,
) -> CommandRequest {
    let clean = match (environment, &request.shell) {
        (Some(environment), _) => matches!(environment, Environment::Clean(_)),
        (None, Some(shell)) => shell.clean_start,
        (None, None) => request.profile == Some(Profile::Clean),
    };
    let profile = if clean {
        CommandProfile::Clean
    } else {
        CommandProfile::User
    };
    CommandRequest::Run {
        command: request.command.clone().unwrap_or_default(),
        cwd: cwd.to_path_buf(),
        profile,
        shell: request
            .shell
            .as_ref()
            .map(|shell| PathBuf::from(&shell.path)),
        terminal: request.tty,
        reload: request.reload,
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
                let configured = configured_login_shell();
                let mode = if request.tty {
                    RunMode::Terminal(terminal_shell(&request, configured.as_deref()))
                } else {
                    RunMode::Captured(environment(configured.as_deref(), request.profile).map_err(
                        |error| {
                            ToolOutput::failure(format!("shell run profile is invalid: {error}"))
                        },
                    )?)
                };
                Ok(Validated::Run {
                    request,
                    cwd: WorkingDirectory {
                        path: cwd,
                        held: None,
                    },
                    mode,
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
        cwd: WorkingDirectory,
        mode: RunMode,
        path_access: PathAccess,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let environment = match mode {
            RunMode::Captured(environment) => environment,
            RunMode::Terminal(shell) => {
                return self
                    .run_terminal(request, cwd, shell, path_access, cancel)
                    .await;
            }
        };
        if let Some(root) = path_access.confining_root(&self.workspace_root)
            && !self
                .resolve_cwd(request.cwd.as_deref())
                .is_ok_and(|current| path_inside(root, &current))
        {
            return ToolOutput::failure(runtime_failure(PATH_OUTSIDE_WORKSPACE));
        }
        let Some(command) = request.command else {
            return ToolOutput::failure(runtime_failure(UNAVAILABLE));
        };
        let (cwd, cwd_directory) = match cwd.still_named() {
            Ok(held) => held,
            Err(code) => return ToolOutput::failure(runtime_failure(&code)),
        };
        let input = StartCaptured {
            command,
            cwd,
            cwd_directory,
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
        if let Err(ended) = self.index_terminal(&session_id, cancel).await {
            return ended;
        }
        let ceiling = effective_interact_yield_time(has_input, request.yield_time_ms);
        if let Some(retained) = self.executions.tombstone_snapshot(&session_id) {
            if has_input {
                return ToolOutput::failure(runtime_failure("ExecutionTerminal"));
            }
            return finish_command(&retained);
        }
        if self.is_terminal(&session_id) {
            let chars = request.chars.unwrap_or_default();
            return self
                .interact_terminal(&session_id, &chars, ceiling, cancel)
                .await;
        }
        if has_input {
            return ToolOutput::failure(runtime_failure("InvalidBackend"));
        }
        match self
            .executions
            .wait(&session_id, Duration::from_millis(ceiling.into()), cancel)
            .await
        {
            Ok(snapshot) => finish_command(&snapshot),
            Err(error) => ToolOutput::failure(runtime_failure(&error.to_string())),
        }
    }

    async fn stop(&self, request: ShellRequest, cancel: &CancellationToken) -> ToolOutput {
        let session_id = request.session_id.unwrap_or_default();
        if let Err(ended) = self.index_terminal(&session_id, cancel).await {
            return ended;
        }
        if let Some(retained) = self.executions.tombstone_snapshot(&session_id) {
            return finish_stop(&retained);
        }
        if self.is_terminal(&session_id) {
            return self.stop_terminal(&session_id, request.force, cancel).await;
        }
        match self.executions.stop(&session_id, request.force).await {
            Ok(snapshot) => finish_stop(&snapshot),
            Err(error) => ToolOutput::failure(runtime_failure(&error.to_string())),
        }
    }
}

fn finish_stop(snapshot: &Snapshot) -> ToolOutput {
    let body = format_snapshot(snapshot, MAX_TOOL_RESULT_BYTES);
    let output = if stop_result_failed(snapshot.state) {
        ToolOutput::failure(body)
    } else {
        ToolOutput::success(body)
    };
    output
        .with_command_result(command_result(snapshot))
        .with_process(process_presentation(snapshot))
}

fn finish_command(snapshot: &Snapshot) -> ToolOutput {
    let body = format_snapshot(snapshot, MAX_TOOL_RESULT_BYTES);
    let output = if snapshot_failed(snapshot) {
        ToolOutput::failure(body)
    } else {
        ToolOutput::success(body)
    };
    output
        .with_command_result(command_result(snapshot))
        .with_process(process_presentation(snapshot))
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

    fn complete(&mut self) {
        if let Ok(Validated::Run { cwd, .. }) = &mut self.validated {
            cwd.hold();
        }
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
                Ok(Validated::Run { request, cwd, mode }) => {
                    shell
                        .run(request, cwd, mode, context.path_access, &cancel)
                        .await
                }
                Ok(Validated::UnresolvedCwd { failure, .. }) => ToolOutput::failure(failure),
                Ok(Validated::Interact(request)) => shell.interact(request, &cancel).await,
                Ok(Validated::Stop(request)) => shell.stop(request, &cancel).await,
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
