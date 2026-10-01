use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use ofx_agent::{
    Agent, AgentConfig, BlockedCall, TurnFailure, TurnReport, normalize_assistant_text_for_display,
    text_for_completed_presentation,
};
use ofx_auth::{CHATGPT_RELOGIN_MESSAGE, CHATGPT_SOURCE_LABEL, MISSING_CHATGPT_CREDENTIAL_MESSAGE};
use ofx_cli::{AskArgs, AskError, AskOutput, LaunchModifiers, read_stdin_prompt};
use ofx_config::{ConnectionError, ProfilePaths, SelectionError, Settings, request_output_tokens};
use ofx_contract::{
    ModelProvider, ModelRecoveryAction, ModelRecoveryCause, PermissionMode, ProviderError,
    RouteRecoveryStatus, ToolCallId, ToolEffect, ToolResultStatus, TurnOutcome, UiEvent, Usage,
};
use ofx_gateway::ChatCompletionsProvider;
use ofx_permissions::PermissionPolicy;
use ofx_session::{SESSIONS_V2_VARIABLE, sessions_v2_variable_is_on};
use ofx_text::encode_terminal_safe;
use rustix::io::Errno;
use serde::{Serialize, Serializer};
use signal_hook::consts::{SIGINT, SIGTERM};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::codex_provider::{CodexUnavailable, SubscriptionEndpoints, codex_provider};
use crate::context::{GATEWAY_SYSTEM_PROMPT, HostRuntimeContext};
use crate::tool_set;

const CONFIGURED_SOURCE_LABEL: &str = "configured provider";
const YOLO_WARNING: &str = "Full access enabled: oh-fx permission checks disabled";
const UNAVAILABLE_CODE: &str = "NotAvailableYet";
const INVALID_MODEL_CODE: &str = "InvalidModel";
const PERMISSION_REQUIRED_HEADLINE: &str =
    "permission required for tool execution in noninteractive mode";
const PERMISSION_PROMPT_UNAVAILABLE: &str = "noninteractive_permission_prompt_unavailable";
const ASK_MODE_APPROVAL_HINT: &str = "rerun with --auto to review this exact action automatically, or use the interactive shell to approve it";
const AUTO_MODE_APPROVAL_HINT: &str = "human approval is required for this action; use the interactive shell to approve it, or add a narrow matching permission rule";

struct Failure {
    code: String,
    notice: Option<String>,
    notice_in_json: bool,
    model: Vec<u8>,
}

impl Failure {
    fn code(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            notice: None,
            notice_in_json: false,
            model: Vec::new(),
        }
    }

    fn invalid_model(model: Vec<u8>) -> Self {
        Self {
            model,
            ..Self::code(INVALID_MODEL_CODE)
        }
    }

    fn notice(code: impl Into<String>, notice: impl Into<String>) -> Self {
        Self {
            notice: Some(notice.into()),
            ..Self::code(code)
        }
    }

    fn written(error: &io::Error) -> Self {
        Self::code(write_error_name(error))
    }

    fn report(&self, json: bool) -> ExitCode {
        let mut stderr = io::stderr().lock();
        if let Some(notice) = self
            .notice
            .as_ref()
            .filter(|_| !json || self.notice_in_json)
        {
            let _ = writeln!(stderr, "oh-fx ask: {notice}");
        } else if !json {
            let _ = writeln!(stderr, "oh-fx: {}", self.code);
        }
        drop(stderr);
        if json {
            return print_result(&RunResult {
                model: JsonText(&self.model),
                ..RunResult::error(&self.code)
            });
        }
        ExitCode::FAILURE
    }
}

impl From<SelectionError> for Failure {
    fn from(error: SelectionError) -> Self {
        match error {
            SelectionError::ModelNotSelected
            | SelectionError::CodexModelNotSelected
            | SelectionError::ProviderUnavailable(_) => {
                Self::notice(error.code(), error.to_string())
            }
            SelectionError::InvalidProviderValue | SelectionError::UnknownConfiguredProvider => {
                Self::code(error.code())
            }
        }
    }
}

impl From<ConnectionError> for Failure {
    fn from(error: ConnectionError) -> Self {
        let notice_in_json = error.code() == "MissingCredentials";
        Self {
            notice_in_json,
            ..Self::notice(error.code(), error.to_string())
        }
    }
}

impl From<CodexUnavailable> for Failure {
    fn from(error: CodexUnavailable) -> Self {
        match error {
            CodexUnavailable::MissingLogin => Self {
                notice_in_json: true,
                ..Self::notice("MissingCredentials", MISSING_CHATGPT_CREDENTIAL_MESSAGE)
            },
            CodexUnavailable::Preparation(error) => Self::notice(error.to_string(), error.notice()),
            CodexUnavailable::Client(error) => {
                Self::notice("TransportUnavailable", error.to_string())
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialSource {
    Configured,
    Codex,
}

impl CredentialSource {
    const fn label(self) -> &'static str {
        match self {
            Self::Configured => CONFIGURED_SOURCE_LABEL,
            Self::Codex => CHATGPT_SOURCE_LABEL,
        }
    }

    const fn relogin(self) -> Option<&'static str> {
        match self {
            Self::Configured => None,
            Self::Codex => Some(CHATGPT_RELOGIN_MESSAGE),
        }
    }
}

struct Route {
    provider: Arc<dyn ModelProvider>,
    model: Result<String, Vec<u8>>,
    max_output_tokens: Option<u32>,
    source: CredentialSource,
}

pub(crate) fn run(args: &AskArgs, modifiers: &LaunchModifiers) -> ExitCode {
    let prompt = match args.resolve_prompt(read_stdin_prompt) {
        Ok(prompt) => prompt,
        Err(error) => return report_argument_error(error),
    };
    if let Some(feature) = unavailable_feature(args, modifiers) {
        return unavailable(&feature, args.output.json);
    }
    if prompt.is_empty() {
        return Failure::code("InvalidConversationEvent").report(args.output.json);
    }
    crate::auto_upgrade::announce_and_schedule();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    match runtime {
        Ok(runtime) => runtime.block_on(ask(args, &prompt)),
        Err(_) => Failure::code("RuntimeUnavailable").report(args.output.json),
    }
}

pub(crate) fn report_argument_error(error: AskError) -> ExitCode {
    if error.json {
        return print_result(&RunResult::error(error.kind.name()));
    }
    let _ = io::stderr().write_all(error.report().stderr.as_bytes());
    ExitCode::FAILURE
}

fn unavailable_feature(args: &AskArgs, modifiers: &LaunchModifiers) -> Option<String> {
    let launch = [
        (modifiers.sets_context_limits(), "--context-limit"),
        (modifiers.adds_directories(), "--add-dir"),
    ];
    let ask = [
        (args.images, "--image"),
        (args.permissions.prompt, "--prompt-permissions"),
        (args.timeout, "--timeout"),
        (args.session.continue_recovery, "--continue-recovery"),
    ];
    if let Some(flag) = first_requested(launch) {
        return Some(flag.to_owned());
    }
    first_requested(ask)
        .or(args.session.resume_flag)
        .map(|flag| format!("ask {flag}"))
        .or_else(|| sessions_v2_source(args, modifiers).map(str::to_owned))
}

fn sessions_v2_source(args: &AskArgs, modifiers: &LaunchModifiers) -> Option<&'static str> {
    if args.session.no_save {
        return None;
    }
    first_requested([
        (modifiers.selects_sessions_v2(), "--sessions-v2"),
        (args.session.sessions_v2, "ask --sessions-v2"),
    ])
    .or_else(|| {
        let variable = env::var(SESSIONS_V2_VARIABLE).ok();
        sessions_v2_variable_is_on(variable.as_deref()).then_some(SESSIONS_V2_VARIABLE)
    })
}

fn first_requested<const N: usize>(flags: [(bool, &'static str); N]) -> Option<&'static str> {
    flags
        .into_iter()
        .find_map(|(requested, flag)| requested.then_some(flag))
}

fn unavailable(feature: &str, json: bool) -> ExitCode {
    crate::write_unavailable(feature);
    if json {
        return print_result(&RunResult::error(UNAVAILABLE_CODE));
    }
    ExitCode::FAILURE
}

async fn ask(args: &AskArgs, prompt: &str) -> ExitCode {
    let cancel = CancellationToken::new();
    let received_signal = watch_signals(cancel.clone());
    let (mut agent, model, permission_mode, source) = match prepare_agent(args).await {
        Ok(prepared) => prepared,
        Err(failure) => return failure.report(args.output.json),
    };
    let mut presenter = Presenter::new(args.output, permission_mode, source);
    let report = agent
        .run_turn(
            prompt,
            &mut |event| {
                if !presenter.handle(event) {
                    cancel.cancel();
                }
            },
            &cancel,
        )
        .await;
    match received_signal.load(Ordering::SeqCst) {
        0 => presenter.finish(&report, &model),
        signal => {
            let _ = io::stdout().flush();
            crate::die_by_signal(signal)
        }
    }
}

async fn prepare_agent(
    args: &AskArgs,
) -> Result<(Agent, String, PermissionMode, CredentialSource), Failure> {
    let workspace_root = workspace_root()?;
    let paths = ProfilePaths::from_environment();
    let settings = match &paths {
        Some(paths) => Settings::load(paths, &workspace_root)
            .map_err(|error| Failure::code(error.to_string()))?,
        None => Settings::default(),
    };
    if settings.profile_is_unusable() {
        return Err(Failure::code("InvalidProfileConfiguration"));
    }
    let permission_mode = args
        .permissions
        .mode
        .unwrap_or_else(|| settings.permission_mode());
    let mut stderr = io::stderr().lock();
    if permission_mode == PermissionMode::Yolo && !settings.yolo_acknowledged() {
        let warning = if !args.output.no_color && stderr.is_terminal() {
            format!("\x1b[38;5;252m{YOLO_WARNING}\x1b[0m")
        } else {
            YOLO_WARNING.to_owned()
        };
        writeln!(stderr, "{warning}").map_err(|error| Failure::written(&error))?;
    }
    for diagnostic in settings.diagnostics() {
        writeln!(stderr, "oh-fx ask: {diagnostic}").map_err(|error| Failure::written(&error))?;
    }
    drop(stderr);
    let lookup = |name: &str| env::var(name).ok();
    let requested = args.model.as_deref();
    let route = if settings.codex_selected(&lookup)? {
        let model = select_model(requested, |model| {
            settings.selected_codex_model(model, &lookup)
        })?;
        let provider = codex_provider(
            paths.map(|paths| paths.data),
            &crate::user_agent(),
            SubscriptionEndpoints::default(),
        )
        .await?;
        Route {
            provider: Arc::new(provider),
            model,
            max_output_tokens: None,
            source: CredentialSource::Codex,
        }
    } else {
        let connection = settings.selected_connection(&lookup)?;
        let model = select_model(requested, |model| {
            settings.selected_model(connection, model, &lookup)
        })?;
        let resolved = connection.resolve(&lookup, env::home_dir().as_deref())?;
        let provider = ChatCompletionsProvider::new(resolved, &crate::user_agent())
            .map_err(|error| Failure::notice("InvalidConnection", error.to_string()))?;
        Route {
            provider: Arc::new(provider),
            max_output_tokens: model
                .as_ref()
                .ok()
                .and_then(|model| request_output_tokens(connection.capabilities(model))),
            model,
            source: CredentialSource::Configured,
        }
    };
    let model = route.model.map_err(Failure::invalid_model)?;
    let config = AgentConfig {
        system_prompt: args
            .system_prompt
            .clone()
            .unwrap_or_else(|| GATEWAY_SYSTEM_PROMPT.to_owned()),
        max_output_tokens: route.max_output_tokens,
        step_limit: settings.max_agent_steps(&lookup),
        model: model.clone(),
    };
    let tools = tool_set::ask_tools(&workspace_root);
    let permissions = PermissionPolicy::new(permission_mode, workspace_root.clone());
    let context = HostRuntimeContext::new(workspace_root, permission_mode);
    let agent = Agent::new(
        route.provider,
        tools,
        Arc::new(context),
        Arc::new(permissions),
        config,
    );
    Ok((agent, model, permission_mode, route.source))
}

fn select_model(
    requested: Option<&OsStr>,
    select: impl FnOnce(Option<&str>) -> Result<String, SelectionError>,
) -> Result<Result<String, Vec<u8>>, SelectionError> {
    match requested {
        Some(requested) if requested.to_str().is_none() => Ok(Err(requested.as_bytes().to_vec())),
        requested => select(requested.and_then(OsStr::to_str)).map(Ok),
    }
}

fn workspace_root() -> Result<PathBuf, Failure> {
    env::current_dir()
        .and_then(fs::canonicalize)
        .map_err(|_| Failure::code("WorkspaceUnavailable"))
}

fn watch_signals(cancel: CancellationToken) -> Arc<AtomicI32> {
    let received = Arc::new(AtomicI32::new(0));
    let flag = Arc::clone(&received);
    tokio::spawn(async move {
        let (Ok(mut interrupt), Ok(mut terminate)) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) else {
            return;
        };
        let received_signal = tokio::select! {
            _ = interrupt.recv() => SIGINT,
            _ = terminate.recv() => SIGTERM,
        };
        flag.store(received_signal, Ordering::SeqCst);
        cancel.cancel();
    });
    received
}

fn write_error_name(error: &io::Error) -> &'static str {
    match Errno::from_io_error(error) {
        Some(Errno::PIPE) => "BrokenPipe",
        Some(Errno::NOSPC) => "NoSpaceLeft",
        Some(Errno::BADF) => "NotOpenForWriting",
        Some(Errno::DQUOT) => "DiskQuota",
        Some(Errno::FBIG) => "FileTooBig",
        Some(Errno::IO) => "InputOutput",
        Some(Errno::PERM) => "PermissionDenied",
        Some(Errno::AGAIN) => "WouldBlock",
        Some(Errno::BUSY) => "DeviceBusy",
        _ => "Unexpected",
    }
}

fn write_stderr(text: &str) -> io::Result<()> {
    let mut stderr = io::stderr().lock();
    stderr.write_all(text.as_bytes())?;
    stderr.flush()
}

fn print_result(result: &RunResult<'_>) -> ExitCode {
    let exit = if result.exit_code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    };
    let written = serde_json::to_string(result)
        .map_err(io::Error::other)
        .and_then(|line| crate::write_stdout(&format!("{line}\n")));
    match written {
        Ok(()) => exit,
        Err(error) => {
            let _ = write_stderr(&format!("oh-fx: {}\n", write_error_name(&error)));
            ExitCode::FAILURE
        }
    }
}

#[derive(Serialize)]
struct ToolRecord {
    name: String,
    status: &'static str,
}

impl ToolRecord {
    fn new(name: String, status: ToolResultStatus) -> Self {
        Self {
            name,
            status: match status {
                ToolResultStatus::Success => "success",
                ToolResultStatus::Failure => "error",
            },
        }
    }
}

#[derive(Serialize)]
struct UsageRecord {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Serialize)]
struct AuthFailureRecord {
    source: &'static str,
    reason: &'static str,
    http_status: u16,
}

#[derive(Serialize)]
struct RecoveryRecord {
    state: &'static str,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<&'static str>,
    attempt: usize,
    attempt_limit: usize,
    delay_seconds: u64,
    durable: bool,
    message: String,
}

impl RecoveryRecord {
    fn new(status: &RouteRecoveryStatus) -> Self {
        Self {
            state: if status.is_recovered() {
                "recovered"
            } else {
                "active"
            },
            kind: status.kind.as_str(),
            cause: status.cause.map(ModelRecoveryCause::as_str),
            action: status.action.map(ModelRecoveryAction::as_str),
            attempt: status.reported_attempt(),
            attempt_limit: status.attempt_limit,
            delay_seconds: status.delay_seconds,
            durable: false,
            message: status.label(),
        }
    }
}

struct JsonText<'a>(&'a [u8]);

impl Serialize for JsonText<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match std::str::from_utf8(self.0) {
            Ok(text) => serializer.serialize_str(text),
            Err(_) => serializer.serialize_bytes(self.0),
        }
    }
}

#[derive(Serialize)]
struct RunResult<'a> {
    output: &'a str,
    final_output: &'a str,
    exit_code: u8,
    model: JsonText<'a>,
    resolved_provider: Option<&'a str>,
    session_id: &'a str,
    steps: u64,
    tool_calls: &'a [ToolRecord],
    usage: UsageRecord,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_failure: Option<AuthFailureRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery: Option<RecoveryRecord>,
}

impl<'a> RunResult<'a> {
    fn error(code: &'a str) -> Self {
        Self {
            output: "",
            final_output: "",
            exit_code: 1,
            model: JsonText(b""),
            resolved_provider: None,
            session_id: "",
            steps: 0,
            tool_calls: &[],
            usage: usage_record(Usage::default()),
            error: Some(code),
            auth_failure: None,
            recovery: None,
        }
    }
}

fn usage_record(usage: Usage) -> UsageRecord {
    UsageRecord {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Json,
    Quiet,
    Raw,
    Terminal,
}

struct Presenter {
    mode: OutputMode,
    permission_mode: PermissionMode,
    source: CredentialSource,
    output: String,
    has_output: bool,
    boundary_pending: bool,
    trailing_newlines: usize,
    steps: u64,
    tool_calls: Vec<ToolRecord>,
    settling_progress: Vec<(ToolCallId, String)>,
    recovery: Option<RouteRecoveryStatus>,
    write_error: Option<&'static str>,
}

struct FailureSummary {
    error: Option<String>,
    auth_failure: bool,
}

impl Presenter {
    fn new(output: AskOutput, permission_mode: PermissionMode, source: CredentialSource) -> Self {
        let mode = if output.json {
            OutputMode::Json
        } else if output.quiet {
            OutputMode::Quiet
        } else if io::stdout().is_terminal() {
            OutputMode::Terminal
        } else {
            OutputMode::Raw
        };
        Self {
            mode,
            permission_mode,
            source,
            output: String::new(),
            has_output: false,
            boundary_pending: false,
            trailing_newlines: 0,
            steps: 0,
            tool_calls: Vec::new(),
            settling_progress: Vec::new(),
            recovery: None,
            write_error: None,
        }
    }

    fn handle(&mut self, event: UiEvent) -> bool {
        let written = match event {
            UiEvent::AssistantText { text, .. } => self.push_assistant(&text),
            UiEvent::Operational { text, .. } => write_stderr(&text),
            UiEvent::Recovery { status, .. } => {
                let notice = (self.mode != OutputMode::Quiet)
                    .then(|| format!("[notice] {}\n", status.label()));
                self.recovery = Some(status);
                notice.map_or(Ok(()), |line| write_stderr(&line))
            }
            UiEvent::ToolStarted {
                call_id,
                description,
                ..
            } => {
                self.start_step();
                let line = self.progress_line(&description.title);
                if description.effect == ToolEffect::None {
                    self.settling_progress.push((call_id, line));
                    Ok(())
                } else {
                    write_stderr(&line)
                }
            }
            UiEvent::ToolFinished {
                call_id,
                tool_name,
                status,
                ..
            } => {
                self.tool_calls.push(ToolRecord::new(tool_name, status));
                match self.take_settling_progress(&call_id) {
                    Some(line) => write_stderr(&line),
                    None => Ok(()),
                }
            }
            UiEvent::ToolRejected { tool_name, .. } => {
                self.start_step();
                self.tool_calls
                    .push(ToolRecord::new(tool_name, ToolResultStatus::Failure));
                Ok(())
            }
            UiEvent::TurnStarted { .. }
            | UiEvent::ReasoningText { .. }
            | UiEvent::UsageReported { .. }
            | UiEvent::TurnFinished { .. } => Ok(()),
        };
        match written {
            Ok(()) => true,
            Err(error) => {
                self.write_error.get_or_insert(write_error_name(&error));
                false
            }
        }
    }

    fn progress_line(&self, title: &str) -> String {
        format!("{}\n", self.display_title(title))
    }

    fn display_title(&self, title: &str) -> String {
        if self.mode == OutputMode::Terminal {
            encode_terminal_safe(title.as_bytes(), usize::MAX).text
        } else {
            title.to_owned()
        }
    }

    fn blocked_action_guidance(&self, title: &str) -> String {
        let hint = if self.permission_mode == PermissionMode::Auto {
            AUTO_MODE_APPROVAL_HINT
        } else {
            ASK_MODE_APPROVAL_HINT
        };
        format!(
            "oh-fx ask: {PERMISSION_REQUIRED_HEADLINE}\noh-fx ask: blocked action: {}\noh-fx ask: reason={PERMISSION_PROMPT_UNAVAILABLE}\noh-fx ask: {hint}\n",
            self.display_title(title)
        )
    }

    fn take_settling_progress(&mut self, call_id: &ToolCallId) -> Option<String> {
        let index = self
            .settling_progress
            .iter()
            .position(|(settling, _)| settling == call_id)?;
        Some(self.settling_progress.remove(index).1)
    }

    fn start_step(&mut self) {
        self.steps += 1;
        if self.has_output {
            self.boundary_pending = true;
        }
    }

    fn push_assistant(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() || self.mode == OutputMode::Quiet {
            return Ok(());
        }
        if self.boundary_pending && self.has_output {
            let separator = match self.trailing_newlines {
                0 => "\n\n",
                1 => "\n",
                _ => "",
            };
            self.write_assistant(separator)?;
        }
        self.boundary_pending = false;
        self.write_assistant(text)
    }

    fn write_assistant(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if self.mode == OutputMode::Json {
            self.output.push_str(text);
        } else {
            crate::write_stdout(text)?;
        }
        self.has_output = true;
        let trailing = text.bytes().rev().take_while(|byte| *byte == b'\n').count();
        self.trailing_newlines = if trailing == text.len() {
            (self.trailing_newlines + trailing.min(2)).min(2)
        } else {
            trailing.min(2)
        };
        Ok(())
    }

    fn describe_failure(&mut self, failure: &TurnFailure) -> FailureSummary {
        let summary = match failure {
            TurnFailure::Provider(error) if error.status.is_some() => {
                self.describe_http_failure(error)
            }
            TurnFailure::Provider(error) => {
                self.describe_error(&error.code, error.detail.as_deref())
            }
            TurnFailure::InvalidCompletion | TurnFailure::PermissionRequired(_) => {
                self.describe_error(failure.code(), None)
            }
            TurnFailure::StepLimitReached => Ok(FailureSummary {
                error: None,
                auth_failure: false,
            }),
        };
        summary.unwrap_or_else(|error| FailureSummary {
            error: Some(write_error_name(&error).to_owned()),
            auth_failure: false,
        })
    }

    fn describe_error(&self, code: &str, detail: Option<&str>) -> io::Result<FailureSummary> {
        if self.mode != OutputMode::Json {
            write_stderr(&format!("oh-fx: {code}\n"))?;
        }
        if let Some(detail) = detail {
            write_stderr(&format!("oh-fx ask: {detail}\n"))?;
        }
        Ok(FailureSummary {
            error: Some(code.to_owned()),
            auth_failure: false,
        })
    }

    fn describe_http_failure(&mut self, error: &ProviderError) -> io::Result<FailureSummary> {
        let status = error.status.unwrap_or_default();
        let bare = format!("HTTP {status}");
        let detail = error.detail.as_deref().unwrap_or(&bare);
        let auth_failure = status == 401;
        let message = if auth_failure {
            format!("{} authentication failed · HTTP 401", self.source.label())
        } else {
            detail.to_owned()
        };
        write_stderr(&format!("oh-fx ask: {message}\n"))?;
        if auth_failure && detail != bare {
            write_stderr(&format!("oh-fx ask: {detail}\n"))?;
        }
        if let Some(guidance) = self.source.relogin().filter(|_| auth_failure) {
            write_stderr(&format!("oh-fx ask: {guidance}\n"))?;
        }
        if self.mode == OutputMode::Json {
            self.output.push_str(&message);
            self.output.push('\n');
        }
        Ok(FailureSummary {
            error: None,
            auth_failure,
        })
    }

    fn finish(mut self, report: &TurnReport, model: &str) -> ExitCode {
        if let (None, Some(failure @ TurnFailure::PermissionRequired(blocked))) =
            (self.write_error, &report.failure)
        {
            return self.finish_blocked(failure.code(), blocked, report.usage);
        }
        let summary = match (self.write_error, &report.failure) {
            (Some(code), _) => FailureSummary {
                error: Some(code.to_owned()),
                auth_failure: false,
            },
            (None, Some(failure)) => self.describe_failure(failure),
            (None, None) => FailureSummary {
                error: None,
                auth_failure: false,
            },
        };
        let completed = report.outcome == TurnOutcome::Completed
            && self.write_error.is_none()
            && summary.error.is_none();
        if self.mode != OutputMode::Json {
            if let Some(code) = self.write_error {
                let _ = write_stderr(&format!("oh-fx: {code}\n"));
            }
            if completed && self.has_output && self.mode == OutputMode::Terminal {
                let _ = crate::write_stdout("\n");
            }
            return if completed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        let normalized = normalize_assistant_text_for_display(&report.final_text);
        let final_output = if completed {
            text_for_completed_presentation(&report.final_text, &normalized)
        } else {
            ""
        };
        print_result(&RunResult {
            output: &self.output,
            final_output,
            exit_code: u8::from(!completed),
            model: JsonText(model.as_bytes()),
            resolved_provider: None,
            session_id: "",
            steps: self.steps,
            tool_calls: &self.tool_calls,
            usage: usage_record(report.usage),
            error: summary.error.as_deref(),
            auth_failure: summary.auth_failure.then_some(AuthFailureRecord {
                source: self.source.label(),
                reason: "http_unauthorized",
                http_status: 401,
            }),
            recovery: self.recovery.as_ref().map(RecoveryRecord::new),
        })
    }

    fn finish_blocked(mut self, code: &str, blocked: &BlockedCall, usage: Usage) -> ExitCode {
        let error = match write_stderr(&self.blocked_action_guidance(&blocked.title)) {
            Ok(()) => code,
            Err(error) => write_error_name(&error),
        };
        self.tool_calls.push(ToolRecord::new(
            blocked.tool_name.clone(),
            ToolResultStatus::Failure,
        ));
        if self.mode != OutputMode::Json {
            return ExitCode::FAILURE;
        }
        print_result(&RunResult {
            output: &self.output,
            tool_calls: &self.tool_calls,
            usage: usage_record(usage),
            ..RunResult::error(error)
        })
    }
}

#[cfg(test)]
mod tests {
    use ofx_auth::PreparationError;
    use ofx_contract::{
        CallDescription, Concurrency, ModelFailureDiagnostic, ProviderErrorKind, RouteRecoveryKind,
        ToolActivity, TurnId,
    };

    use super::*;

    fn result_json(result: &RunResult<'_>) -> String {
        serde_json::to_string(result).unwrap()
    }

    #[test]
    fn error_results_match_the_upstream_envelope() {
        assert_eq!(
            result_json(&RunResult::error("MissingPrompt")),
            r#"{"output":"","final_output":"","exit_code":1,"model":"","resolved_provider":null,"session_id":"","steps":0,"tool_calls":[],"usage":{"input_tokens":null,"output_tokens":null},"error":"MissingPrompt"}"#
        );
    }

    #[test]
    fn tool_records_and_recovery_follow_the_upstream_shape() {
        let records = [
            ToolRecord::new("read_file".to_owned(), ToolResultStatus::Success),
            ToolRecord::new("read_file".to_owned(), ToolResultStatus::Failure),
        ];
        let recovered = RouteRecoveryStatus {
            kind: RouteRecoveryKind::AutoRecovered,
            failed_attempt: 0,
            succeeded_attempt: 2,
            attempt_limit: 10,
            cause: None,
            action: None,
            delay_seconds: 0,
            diagnostic: None,
        };
        let result = RunResult {
            tool_calls: &records,
            error: None,
            recovery: Some(RecoveryRecord::new(&recovered)),
            ..RunResult::error("")
        };
        assert_eq!(
            result_json(&result),
            r#"{"output":"","final_output":"","exit_code":1,"model":"","resolved_provider":null,"session_id":"","steps":0,"tool_calls":[{"name":"read_file","status":"success"},{"name":"read_file","status":"error"}],"usage":{"input_tokens":null,"output_tokens":null},"recovery":{"state":"recovered","kind":"auto_recovered","attempt":2,"attempt_limit":10,"delay_seconds":0,"durable":false,"message":"✓ recovered · succeeded on attempt 2"}}"#
        );
        let retrying = RouteRecoveryStatus {
            kind: RouteRecoveryKind::AutoRetry,
            failed_attempt: 2,
            succeeded_attempt: 0,
            attempt_limit: 10,
            cause: Some(ModelRecoveryCause::RateLimited),
            action: Some(ModelRecoveryAction::RetryingRequest),
            delay_seconds: 2,
            diagnostic: Some(ModelFailureDiagnostic::new("HTTP 429 · slow")),
        };
        assert_eq!(
            serde_json::to_string(&RecoveryRecord::new(&retrying)).unwrap(),
            r#"{"state":"active","kind":"auto_retry","cause":"rate_limited","action":"retrying_request","attempt":2,"attempt_limit":10,"delay_seconds":2,"durable":false,"message":"⚠ Rate limited · HTTP 429 · slow · retrying request in 2s"}"#
        );
    }

    fn json_presenter() -> Presenter {
        Presenter::new(
            AskOutput {
                json: true,
                ..AskOutput::default()
            },
            PermissionMode::Auto,
            CredentialSource::Configured,
        )
    }

    #[test]
    fn terminal_progress_lines_escape_control_sequences_from_tool_arguments() {
        let title = "Reading \x1b]0;PWNED-TITLE\x07\x1b[2J\x1b[31mred\nnext";
        let mut presenter = json_presenter();
        assert_eq!(presenter.progress_line(title), format!("{title}\n"));
        presenter.mode = OutputMode::Quiet;
        assert_eq!(presenter.progress_line(title), format!("{title}\n"));
        presenter.mode = OutputMode::Raw;
        assert_eq!(presenter.progress_line(title), format!("{title}\n"));
        presenter.mode = OutputMode::Terminal;
        assert_eq!(
            presenter.progress_line(title),
            "Reading \\x1b]0;PWNED-TITLE\\x07\\x1b[2J\\x1b[31mred\\x0anext\n"
        );
        assert_eq!(
            presenter.progress_line("Reading notes.txt"),
            "Reading notes.txt\n"
        );
    }

    #[test]
    fn blocked_actions_print_upstream_guidance_for_the_permission_mode() {
        let mut presenter = json_presenter();
        presenter.permission_mode = PermissionMode::Ask;
        assert_eq!(
            presenter.blocked_action_guidance("Reading /etc/hosts"),
            "oh-fx ask: permission required for tool execution in noninteractive mode\noh-fx ask: blocked action: Reading /etc/hosts\noh-fx ask: reason=noninteractive_permission_prompt_unavailable\noh-fx ask: rerun with --auto to review this exact action automatically, or use the interactive shell to approve it\n"
        );
        presenter.permission_mode = PermissionMode::Auto;
        presenter.mode = OutputMode::Terminal;
        assert_eq!(
            presenter.blocked_action_guidance("Reading /tmp/\x1b[2Jx"),
            "oh-fx ask: permission required for tool execution in noninteractive mode\noh-fx ask: blocked action: Reading /tmp/\\x1b[2Jx\noh-fx ask: reason=noninteractive_permission_prompt_unavailable\noh-fx ask: human approval is required for this action; use the interactive shell to approve it, or add a narrow matching permission rule\n"
        );
    }

    #[test]
    fn codex_unauthorized_failures_name_the_subscription_source() {
        let mut presenter = Presenter::new(
            AskOutput {
                json: true,
                ..AskOutput::default()
            },
            PermissionMode::Auto,
            CredentialSource::Codex,
        );
        let error = ProviderError {
            status: Some(401),
            ..ProviderError::new(ProviderErrorKind::Unauthorized, "HttpError")
                .with_detail("API access denied · HTTP 401 · invalid_token")
        };
        let summary = presenter.describe_failure(&TurnFailure::Provider(error));
        assert!(summary.auth_failure);
        assert_eq!(summary.error, None);
        assert_eq!(
            presenter.output,
            "Codex subscription authentication failed · HTTP 401\n"
        );
        assert_eq!(CredentialSource::Codex.label(), "Codex subscription");
        assert_eq!(
            CredentialSource::Codex.relogin(),
            Some("Run oh-fx login codex to sign in again.")
        );
        assert_eq!(CredentialSource::Configured.relogin(), None);
    }

    #[test]
    fn codex_preparation_failures_follow_the_upstream_codes_and_notices() {
        let missing = Failure::from(CodexUnavailable::MissingLogin);
        assert_eq!(missing.code, "MissingCredentials");
        assert_eq!(
            missing.notice.as_deref(),
            Some("oh-fx needs a Codex subscription login for this model. Run oh-fx login codex.")
        );
        assert!(missing.notice_in_json);
        let storage = Failure::from(CodexUnavailable::Preparation(
            PreparationError::CredentialStorageUnavailable,
        ));
        assert_eq!(storage.code, "CredentialStorageUnavailable");
        assert_eq!(
            storage.notice.as_deref(),
            Some(
                "Saved credential storage is unavailable. Check the saved credential, then retry."
            )
        );
        assert!(!storage.notice_in_json);
        let unselected = Failure::from(SelectionError::CodexModelNotSelected);
        assert_eq!(unselected.code, "CodexModelNotSelected");
        assert!(unselected.notice.is_some());
    }

    #[test]
    fn raw_output_separates_text_around_tool_steps_and_counts_rejections() {
        let mut presenter = json_presenter();
        presenter.push_assistant("Looking.").unwrap();
        assert!(presenter.handle(UiEvent::ToolStarted {
            turn_id: TurnId::new(1),
            call_id: ToolCallId::new("call-1"),
            tool_name: "read_file".to_owned(),
            description: CallDescription {
                title: "Reading".to_owned(),
                activity: ToolActivity::Read,
                effect: ToolEffect::ReadOnly,
                concurrency: Concurrency::Parallel,
            },
        }));
        presenter.push_assistant("Found it.").unwrap();
        assert!(presenter.handle(UiEvent::ToolRejected {
            turn_id: TurnId::new(1),
            call_id: ToolCallId::new("call-2"),
            tool_name: "missing".to_owned(),
        }));
        presenter.push_assistant("\nDone").unwrap();
        assert_eq!(presenter.output, "Looking.\n\nFound it.\n\n\nDone");
        assert_eq!(presenter.steps, 2);
        assert_eq!(presenter.tool_calls.len(), 1);
    }
}
