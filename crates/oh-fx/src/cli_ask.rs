use std::env;
use std::io::{self, IsTerminal, Write};
use std::mem;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;
use std::time::Duration;

use ofx_agent::{
    Agent, BlockedCall, TurnFailure, TurnReport, normalize_assistant_text_for_display,
    text_for_completed_presentation,
};
use ofx_app::{
    CodexUnavailable, ConnectError, CredentialSource, Launch, Profile, ResumeFailure,
    ResumedSession, SubscriptionEndpoints, TitleGeneration, WebFetchProgress, default_mode,
    open_store, recovered_turn,
};
use ofx_auth::MISSING_CHATGPT_CREDENTIAL_MESSAGE;
use ofx_cli::{AskArgs, AskError, AskOutput, LaunchModifiers, read_stdin_prompt};
use ofx_config::{
    ConnectionError, ContextLimitName, ContextLimitOverride, ProfilePaths, SelectionError,
    Settings, save_yolo_acknowledged,
};
use ofx_contract::{
    CallDescription, FULL_ACCESS_WARNING, ModelRecoveryAction, ModelRecoveryCause, PermissionMode,
    RecoveredTurn, RouteRecoveryStatus, ToolActivity, ToolCallId, ToolEffect, ToolRejection,
    ToolResultStatus, TurnOutcome, UiEvent, Usage, format_unknown_action,
};
use ofx_exec::{ManagedExecutions, SessionSupervisor};
use ofx_gateway::HttpFailure;
use ofx_mcp::{McpRuntime, ShutdownMode, StartupPhase, render_workspace_diagnostic};
use ofx_session::{
    SESSIONS_V2_VARIABLE, SessionError, SessionPreferences, sessions_v2_variable_is_on,
};
use ofx_text::encode_terminal_safe;
use rustix::io::Errno;
use serde::{Serialize, Serializer};
use serde_json::Value;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use tokio_util::sync::CancellationToken;

use crate::ask_session::SavedAsk;
use crate::command_echo::CommandEcho;
use crate::question_call_record::question_text;
use crate::shell_call_record::{
    CallError, ShellFailure, failed_call, preflight_failed_call, rejected_call,
};

const UNAVAILABLE_CODE: &str = "NotAvailableYet";
const WEB_FETCH_TOOL: &str = "web_fetch";
const INVALID_MODEL_CODE: &str = "InvalidModel";
const HOME_NOT_SET: &str = "HomeNotSet";
const PERMISSION_REQUIRED_HEADLINE: &str =
    "permission required for tool execution in noninteractive mode";
const PERMISSION_PROMPT_UNAVAILABLE: &str = "noninteractive_permission_prompt_unavailable";
const ASK_MODE_APPROVAL_HINT: &str = "rerun with --auto to review this exact action automatically, or use the interactive shell to approve it";
const AUTO_MODE_APPROVAL_HINT: &str = "human approval is required for this action; use the interactive shell to approve it, or add a narrow matching permission rule";
const BLANK_TEXT: [char; 4] = [' ', '\t', '\r', '\n'];
const UNSAVED_RECOVERY: &str =
    "This run was started with --no-save, so its recovery context cannot be resumed after exit.";
const APPLIED_LIMITS: [ContextLimitName; 6] = [
    ContextLimitName::SkillDescriptionBytes,
    ContextLimitName::SkillCatalogBytes,
    ContextLimitName::SkillChunkBytes,
    ContextLimitName::SkillFileBytes,
    ContextLimitName::ProjectInstructionFileBytes,
    ContextLimitName::ProjectInstructionsTotalBytes,
];

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
            SelectionError::InvalidProviderValue
            | SelectionError::UnknownConfiguredProvider
            | SelectionError::ConfiguredProviderChanged => Self::code(error.code()),
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

impl From<SessionError> for Failure {
    fn from(error: SessionError) -> Self {
        Self::code(error.to_string())
    }
}

impl From<ResumeFailure> for Failure {
    fn from(failure: ResumeFailure) -> Self {
        match failure {
            ResumeFailure::Session(error) => error.into(),
            ResumeFailure::Selection(error) => error.into(),
        }
    }
}

impl From<ConnectError> for Failure {
    fn from(error: ConnectError) -> Self {
        match error {
            ConnectError::Selection(error) => error.into(),
            ConnectError::Connection(error) => error.into(),
            ConnectError::InvalidConnection(error) => {
                Self::notice("InvalidConnection", error.to_string())
            }
            ConnectError::Codex(error) => error.into(),
            ConnectError::InvalidModel(model) => Self::invalid_model(model),
            ConnectError::Mcp(error) => Self::code(error.to_string()),
        }
    }
}

#[derive(Default)]
struct ReceivedSignals {
    first: Arc<AtomicI32>,
    interrupt: Arc<AtomicBool>,
    terminate: Arc<AtomicBool>,
}

impl ReceivedSignals {
    fn received(&self) -> Option<i32> {
        match self.first.load(Ordering::SeqCst) {
            0 => [(SIGINT, &self.interrupt), (SIGTERM, &self.terminate)]
                .into_iter()
                .find_map(|(signal, flag)| flag.load(Ordering::SeqCst).then_some(signal)),
            signal => Some(signal),
        }
    }

    fn unless_signalled<T>(&self, value: T) -> Result<T, Signalled> {
        match self.received() {
            Some(signal) => Err(Signalled(signal)),
            None => Ok(value),
        }
    }
}

struct Signalled(i32);

struct AskRequest<'a> {
    args: &'a AskArgs,
    prompt: &'a str,
    context_limits: &'a [ContextLimitOverride],
    executions: &'a ManagedExecutions,
}

struct PreparedAsk {
    agent: Agent,
    model: String,
    permission_mode: PermissionMode,
    source: CredentialSource,
    context_notices: Vec<String>,
    saved: Option<SavedAsk>,
    title: Option<TitleGeneration>,
    recovered: Option<RecoveredTurn>,
}

pub(crate) fn run(args: &AskArgs, modifiers: &LaunchModifiers) -> ExitCode {
    let prompt = match args.resolve_prompt(read_stdin_prompt) {
        Ok(prompt) => prompt,
        Err(error) => return report_argument_error(error),
    };
    if let Some(feature) = unavailable_feature(args, modifiers) {
        return unavailable(&feature, args.output.json);
    }
    if prompt.is_empty() && !args.session.continue_recovery {
        return Failure::code("InvalidConversationEvent").report(args.output.json);
    }
    crate::auto_upgrade::announce_and_schedule();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    match runtime {
        Ok(runtime) => runtime.block_on(ask(
            args,
            &prompt,
            modifiers.context_limit_overrides(),
            SubscriptionEndpoints::default(),
        )),
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

pub(crate) fn unsupported_launch_modifier(modifiers: &LaunchModifiers) -> Option<&'static str> {
    let unsupported_limit = modifiers
        .context_limit_overrides()
        .iter()
        .any(|limit| !APPLIED_LIMITS.contains(&limit.name));
    first_requested([
        (unsupported_limit, "--context-limit"),
        (modifiers.adds_directories(), "--add-dir"),
    ])
}

fn unavailable_feature(args: &AskArgs, modifiers: &LaunchModifiers) -> Option<String> {
    let ask = [
        (args.images, "--image"),
        (args.permissions.prompt, "--prompt-permissions"),
    ];
    if let Some(flag) = unsupported_launch_modifier(modifiers) {
        return Some(flag.to_owned());
    }
    first_requested(ask)
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
    .or_else(sessions_v2_variable)
}

pub(crate) fn sessions_v2_variable() -> Option<&'static str> {
    let variable = env::var(SESSIONS_V2_VARIABLE).ok();
    sessions_v2_variable_is_on(variable.as_deref()).then_some(SESSIONS_V2_VARIABLE)
}

pub(crate) fn first_requested<const N: usize>(
    flags: [(bool, &'static str); N],
) -> Option<&'static str> {
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

async fn ask(
    args: &AskArgs,
    prompt: &str,
    context_limits: &[ContextLimitOverride],
    endpoints: SubscriptionEndpoints,
) -> ExitCode {
    let Ok(supervisor) = SessionSupervisor::current_executable() else {
        return Failure::code("SelfExeNotFound").report(args.output.json);
    };
    let echo = (output_mode(args.output) != OutputMode::Terminal).then(Arc::default);
    let mut executions = ManagedExecutions::new(supervisor);
    if let Some(echo) = &echo {
        executions = executions.with_output_echo(CommandEcho::output(echo));
    }
    let cancel = CancellationToken::new();
    let received = watch_signals(cancel.clone());
    let request = AskRequest {
        args,
        prompt,
        context_limits,
        executions: &executions,
    };
    let answered = answer(&request, endpoints, echo, &cancel, &received).await;
    executions.shutdown().await;
    settle(answered, &received)
}

fn settle(answered: Result<ExitCode, Signalled>, received: &ReceivedSignals) -> ExitCode {
    match answered.and_then(|exit| received.unless_signalled(exit)) {
        Ok(exit) => exit,
        Err(Signalled(signal)) => {
            let _ = io::stdout().flush();
            crate::die_by_signal(signal)
        }
    }
}

async fn answer(
    request: &AskRequest<'_>,
    endpoints: SubscriptionEndpoints,
    echo: Option<Arc<CommandEcho>>,
    cancel: &CancellationToken,
    received: &ReceivedSignals,
) -> Result<ExitCode, Signalled> {
    let mut mcp = None;
    let answered = respond(request, endpoints, echo, cancel, received, &mut mcp).await;
    if let Some(mcp) = mcp {
        mcp.shutdown(ShutdownMode::Immediate).await;
    }
    answered
}

async fn respond(
    request: &AskRequest<'_>,
    endpoints: SubscriptionEndpoints,
    echo: Option<Arc<CommandEcho>>,
    cancel: &CancellationToken,
    received: &ReceivedSignals,
    mcp: &mut Option<Arc<McpRuntime>>,
) -> Result<ExitCode, Signalled> {
    let args = request.args;
    let prepared =
        received.unless_signalled(prepare_agent(request, endpoints, cancel, mcp).await)?;
    let PreparedAsk {
        mut agent,
        model,
        permission_mode,
        source,
        context_notices,
        saved,
        title,
        recovered,
    } = match prepared {
        Ok(prepared) => prepared,
        Err(failure) => return Ok(failure.report(args.output.json)),
    };
    let mut presenter = Presenter::new(args.output, permission_mode, source)
        .echoing(echo)
        .saving(saved.is_some());
    for notice in &context_notices {
        if !presenter.context_notice(notice) {
            cancel.cancel();
        }
    }
    let titling = title.map(|title| {
        let cancel = cancel.clone();
        tokio::spawn(async move { title.run(&cancel).await })
    });
    let mut present = |event| {
        if !presenter.handle(event) {
            cancel.cancel();
        }
    };
    let report = match recovered {
        Some(recovered) => agent.continue_turn(recovered, &mut present, cancel).await,
        None => agent.run_turn(request.prompt, &mut present, cancel).await,
    };
    if let Some(titling) = titling {
        let _ = titling.await;
    }
    drop(agent);
    let report = received.unless_signalled(report)?;
    Ok(presenter.finish(&report, &model, saved))
}

async fn prepare_agent(
    request: &AskRequest<'_>,
    endpoints: SubscriptionEndpoints,
    cancel: &CancellationToken,
    mcp: &mut Option<Arc<McpRuntime>>,
) -> Result<PreparedAsk, Failure> {
    let args = request.args;
    let mut profile = Profile::load().map_err(|error| Failure::code(error.to_string()))?;
    let permission_mode = args.permissions.mode.unwrap_or_else(|| {
        profile
            .settings()
            .permission_mode(&|name| env::var(name).ok())
    });
    announce_settings(args, &profile, permission_mode)?;
    let mut pending = None;
    let resumed = match &args.session.resume {
        Some(target) => {
            let store = open_store(&profile)?;
            let (resumed, recovery) = ResumedSession::open_for_ask(
                &store,
                &mut profile,
                target,
                args.session.continue_recovery,
            )?;
            pending = recovery;
            Some((store, resumed))
        }
        None => None,
    };
    let (reasoning_effort, fast_mode) = requested_reasoning(
        args,
        profile.settings(),
        resumed.as_ref().map(|(_, resumed)| resumed.preferences()),
    );
    let launch = Launch {
        model: args.model.as_deref(),
        permission_mode,
        system_prompt: args.system_prompt.clone(),
        reasoning_effort,
        fast_mode,
        context_limits: request.context_limits,
        command_timeout: args.timeout_ms.map(Duration::from_millis),
        executions: request.executions,
        endpoints,
        web_fetch_progress: web_fetch_progress(output_mode(args.output)),
        mode: Some(default_mode()),
    };
    let setup = profile.connect(launch, cancel).await?;
    let recovered = pending
        .map(|pending| recovered_turn(pending, &setup))
        .transpose()?;
    *mcp = setup.mcp().cloned();
    if let Some(mcp) = setup.mcp() {
        start_mcp(mcp, cancel).await?;
    }
    let store = match &resumed {
        Some(_) => None,
        None if args.session.no_save => None,
        None => match open_store(&profile) {
            Ok(store) => Some(store),
            Err(error) => {
                write_stderr(&format!(
                    "oh-fx ask: warning: session persistence unavailable; error={error}; continuing without saving\n"
                ))
                .map_err(|error| Failure::written(&error))?;
                None
            }
        },
    };
    let mut agent = setup.agent(resumed.is_some() || store.is_some());
    let saved = match (resumed, store) {
        (Some((store, resumed)), _) => Some(SavedAsk::resume(store, resumed, &setup, &mut agent)?),
        (None, Some(store)) => Some(SavedAsk::start(store, &profile, &setup, &mut agent)?),
        (None, None) => None,
    };
    let prompt = recovered
        .as_ref()
        .map_or(request.prompt, |recovered| recovered.prompt.as_str());
    if let Some(saved) = &saved {
        saved.observe_prompt(prompt);
    }
    let title = saved
        .as_ref()
        .and_then(|saved| saved.title_generation(&setup, prompt, &agent));
    Ok(PreparedAsk {
        agent,
        model: setup.model().to_owned(),
        permission_mode,
        source: setup.source(),
        context_notices: setup.context_notices().to_vec(),
        saved,
        title,
        recovered,
    })
}

async fn start_mcp(mcp: &McpRuntime, cancel: &CancellationToken) -> Result<(), Failure> {
    let mut lines = String::new();
    for diagnostic in &mcp.workspace_diagnostics() {
        lines.push_str("oh-fx ask: ");
        lines.push_str(&render_workspace_diagnostic(diagnostic));
        lines.push('\n');
    }
    let pending = mcp.pending_workspace_names();
    if !pending.is_empty() {
        let names: Vec<String> = pending
            .iter()
            .map(|name| encode_terminal_safe(name.as_bytes(), usize::MAX).text)
            .collect();
        lines.push_str("oh-fx ask: skipped unapproved project MCP servers: ");
        lines.push_str(&names.join(", "));
        lines.push_str(". Approve with oh-fx mcp trust approve <name> before retrying.\n");
    }
    write_stderr(&lines).map_err(|error| Failure::written(&error))?;
    let mut connecting = mcp.connect(StartupPhase::All);
    tokio::select! {
        () = &mut connecting => {}
        () = cancel.cancelled() => {
            connecting.abandon().await;
            return Ok(());
        }
    }
    match mcp.required_startup_failure() {
        Some(failure) => Err(Failure::notice("McpRequiredServerUnavailable", failure)),
        None => Ok(()),
    }
}

fn announce_settings(
    args: &AskArgs,
    profile: &Profile,
    permission_mode: PermissionMode,
) -> Result<(), Failure> {
    let settings = profile.settings();
    let mut stderr = io::stderr().lock();
    if permission_mode == PermissionMode::Yolo && !settings.yolo_acknowledged() {
        let warning = if !args.output.no_color && stderr.is_terminal() {
            format!("\x1b[38;5;252m{FULL_ACCESS_WARNING}\x1b[0m")
        } else {
            FULL_ACCESS_WARNING.to_owned()
        };
        writeln!(stderr, "{warning}").map_err(|error| Failure::written(&error))?;
        if let Err(error) = acknowledge_full_access(profile.paths()) {
            writeln!(
                stderr,
                "oh-fx ask: failed to save full access acknowledgment: {error}"
            )
            .map_err(|error| Failure::written(&error))?;
        }
    }
    for diagnostic in settings.diagnostics() {
        writeln!(stderr, "oh-fx ask: {diagnostic}").map_err(|error| Failure::written(&error))?;
    }
    Ok(())
}

fn acknowledge_full_access(paths: Option<&ProfilePaths>) -> Result<(), String> {
    let paths = paths.ok_or_else(|| HOME_NOT_SET.to_owned())?;
    save_yolo_acknowledged(paths).map_err(|failure| failure.error.to_string())
}

fn requested_reasoning(
    args: &AskArgs,
    settings: &Settings,
    resumed: Option<&SessionPreferences>,
) -> (Option<String>, Option<bool>) {
    let effort = args.effort.clone().unwrap_or_else(|| {
        resumed.map_or_else(
            || settings.reasoning_effort(),
            |preferences| preferences.effort.clone(),
        )
    });
    let fast_mode = args
        .fast
        .or_else(|| resumed.map(|preferences| preferences.fast_mode));
    (effort.into_named(), fast_mode)
}

fn watch_signals(cancel: CancellationToken) -> ReceivedSignals {
    let received = ReceivedSignals::default();
    let _ = signal_hook::flag::register(SIGINT, Arc::clone(&received.interrupt));
    let _ = signal_hook::flag::register(SIGTERM, Arc::clone(&received.terminate));
    let Ok(mut signals) = Signals::new([SIGINT, SIGTERM]) else {
        return received;
    };
    if let Some(signal) = received.received() {
        keep_first(&received.first, signal);
        cancel.cancel();
    }
    let first = Arc::clone(&received.first);
    let _ = thread::Builder::new()
        .name("oh-fx-signals".to_owned())
        .spawn(move || {
            for signal in signals.forever() {
                keep_first(&first, signal);
                cancel.cancel();
            }
        });
    received
}

fn keep_first(first: &AtomicI32, signal: i32) {
    let _ = first.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
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

fn without_leading_blank_lines(text: &str) -> &str {
    let blank = text.len() - text.trim_start_matches(BLANK_TEXT).len();
    text[..blank]
        .rfind('\n')
        .map_or(text, |end| &text[end + 1..])
}

fn web_fetch_progress(mode: OutputMode) -> Option<WebFetchProgress> {
    if mode == OutputMode::Terminal {
        return None;
    }
    Some(Arc::new(|line: &str| {
        let _ = write_stderr(&format!("{line}\n"));
    }))
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
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<CallError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    question: Option<String>,
}

impl ToolRecord {
    fn new(name: String, arguments: &str, status: ToolResultStatus) -> Self {
        Self {
            question: question_text(&name, arguments),
            name,
            status: match status {
                ToolResultStatus::Success => "success",
                ToolResultStatus::Failure => "error",
            },
            action: None,
            error: None,
            command_result: None,
        }
    }

    fn finished(
        name: String,
        arguments: &str,
        status: ToolResultStatus,
        content: &str,
        command_result: Option<&str>,
    ) -> Self {
        let failure = (status == ToolResultStatus::Failure)
            .then(|| failed_call(&name, arguments, content, command_result))
            .flatten();
        Self {
            command_result: command_result.and_then(|result| serde_json::from_str(result).ok()),
            ..Self::new(name, arguments, status).with_failure(failure)
        }
    }

    fn rejected(name: String, arguments: &str) -> Self {
        let failure = rejected_call(&name, arguments);
        Self::new(name, arguments, ToolResultStatus::Failure).with_failure(failure)
    }

    fn preflight_failed(name: String, arguments: &str) -> Self {
        let failure = preflight_failed_call(&name, arguments);
        Self::new(name, arguments, ToolResultStatus::Failure).with_failure(failure)
    }

    fn with_failure(self, failure: Option<ShellFailure>) -> Self {
        match failure {
            Some(failure) => Self {
                action: failure.action,
                error: Some(failure.error),
                ..self
            },
            None => self,
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
    fn new(status: &RouteRecoveryStatus, durable: bool) -> Self {
        Self {
            state: if status.is_terminal() {
                "failed"
            } else if status.is_recovered() {
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
            durable,
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

fn output_mode(output: AskOutput) -> OutputMode {
    if output.json {
        OutputMode::Json
    } else if output.quiet {
        OutputMode::Quiet
    } else if io::stdout().is_terminal() {
        OutputMode::Terminal
    } else {
        OutputMode::Raw
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusBlock {
    Progress,
    Notice,
    Operational,
}

struct Presenter {
    mode: OutputMode,
    claimed_notices: Vec<String>,
    permission_mode: PermissionMode,
    source: CredentialSource,
    stdout: Box<dyn Write + Send>,
    output: String,
    has_output: bool,
    boundary_pending: bool,
    trailing_newlines: usize,
    open_status_block: Option<StatusBlock>,
    held_blank_text: String,
    steps: u64,
    tool_calls: Vec<ToolRecord>,
    settling_progress: Vec<(ToolCallId, String)>,
    recovery: Option<RouteRecoveryStatus>,
    saving: bool,
    write_error: Option<&'static str>,
    command_echo: Option<Arc<CommandEcho>>,
    command_calls: Vec<ToolCallId>,
}

struct FailureSummary {
    error: Option<String>,
    auth_failure: bool,
}

impl Presenter {
    fn new(output: AskOutput, permission_mode: PermissionMode, source: CredentialSource) -> Self {
        Self {
            mode: output_mode(output),
            claimed_notices: Vec::new(),
            permission_mode,
            source,
            stdout: Box::new(io::stdout()),
            output: String::new(),
            has_output: false,
            boundary_pending: false,
            trailing_newlines: 0,
            open_status_block: None,
            held_blank_text: String::new(),
            steps: 0,
            tool_calls: Vec::new(),
            settling_progress: Vec::new(),
            recovery: None,
            saving: false,
            write_error: None,
            command_echo: None,
            command_calls: Vec::new(),
        }
    }

    fn echoing(mut self, echo: Option<Arc<CommandEcho>>) -> Self {
        self.command_echo = echo;
        self
    }

    fn saving(mut self, saving: bool) -> Self {
        self.saving = saving;
        self
    }

    fn recovery_notices(&self, status: &RouteRecoveryStatus) -> Vec<String> {
        let terminal = status.is_terminal();
        if self.mode == OutputMode::Quiet && !terminal {
            return Vec::new();
        }
        let mut notices = vec![format!("[notice] {}\n", status.label())];
        if terminal && !self.saving {
            notices.push(format!("[notice] {UNSAVED_RECOVERY}\n"));
        }
        notices
    }

    fn handle(&mut self, event: UiEvent) -> bool {
        let written = match event {
            UiEvent::AssistantText { text, .. } => self.push_assistant(&text),
            UiEvent::Operational { text, .. } => self.write_status(StatusBlock::Operational, &text),
            UiEvent::Recovery { status, .. } => {
                let notices = self.recovery_notices(&status);
                self.recovery = Some(status);
                notices
                    .iter()
                    .try_for_each(|line| self.write_status(StatusBlock::Notice, line))
            }
            UiEvent::ToolStarted {
                call_id,
                tool_name,
                description,
                ..
            } => self.tool_started(call_id, &tool_name, &description),
            UiEvent::ToolFinished {
                call_id,
                tool_name,
                arguments,
                status,
                content,
                command_result,
                ..
            } => {
                self.tool_calls.push(ToolRecord::finished(
                    tool_name,
                    &arguments,
                    status,
                    &content,
                    command_result.as_deref(),
                ));
                match self.take_settling_progress(&call_id) {
                    Some(line) => self.write_status(StatusBlock::Progress, &line),
                    None => Ok(()),
                }
                .and_then(|()| self.finish_command_output(&call_id))
            }
            UiEvent::ToolRejected {
                tool_name,
                arguments,
                reason,
                description,
                ..
            } => self.tool_rejected(tool_name, &arguments, reason, description),
            UiEvent::ContextNotice { text, .. } => {
                return self.context_notice(&text);
            }
            UiEvent::SystemNotice { text } => {
                self.write_status(StatusBlock::Notice, &format!("[notice] {text}\n"))
            }
            UiEvent::TurnStarted { .. }
            | UiEvent::ToolDeferred { .. }
            | UiEvent::SubagentStatus { .. }
            | UiEvent::SteeringApplied { .. }
            | UiEvent::ReasoningText { .. }
            | UiEvent::UsageReported { .. }
            | UiEvent::TurnFinished { .. }
            | UiEvent::ApprovalRequested { .. }
            | UiEvent::QuestionRequested { .. }
            | UiEvent::ApiStatus { .. }
            | UiEvent::Notice { .. }
            | UiEvent::ModelSelected { .. }
            | UiEvent::SessionTitleChanged { .. }
            | UiEvent::ModelCatalog { .. }
            | UiEvent::PermissionModeChanged { .. }
            | UiEvent::StatuslineChanged { .. }
            | UiEvent::StatuslineMenuOpened
            | UiEvent::HelpRequested
            | UiEvent::StatsRequested
            | UiEvent::CompactionActivity { .. }
            | UiEvent::TurnCompaction { .. }
            | UiEvent::SkillsMenu { .. }
            | UiEvent::ConversationCleared { .. }
            | UiEvent::SessionPickerOpened { .. }
            | UiEvent::SessionsListed { .. }
            | UiEvent::SessionsUnavailable { .. }
            | UiEvent::SessionResumeFailed { .. }
            | UiEvent::SessionResumed { .. }
            | UiEvent::ExitRequested => Ok(()),
        };
        match written {
            Ok(()) => true,
            Err(error) => {
                self.write_error.get_or_insert(write_error_name(&error));
                false
            }
        }
    }

    fn tool_started(
        &mut self,
        call_id: ToolCallId,
        tool_name: &str,
        description: &CallDescription,
    ) -> io::Result<()> {
        self.start_step();
        if description.activity == ToolActivity::Command {
            self.command_calls.push(call_id.clone());
        }
        if self.mode != OutputMode::Terminal && tool_name == WEB_FETCH_TOOL {
            return Ok(());
        }
        let line = self.progress_line(&description.title);
        if description.effect == ToolEffect::None {
            self.settling_progress.push((call_id, line));
            Ok(())
        } else {
            self.write_status(StatusBlock::Progress, &line)
        }
    }

    fn tool_rejected(
        &mut self,
        tool_name: String,
        arguments: &str,
        reason: ToolRejection,
        description: Option<CallDescription>,
    ) -> io::Result<()> {
        self.start_step();
        let title = match reason {
            ToolRejection::Unsupported => Some(format_unknown_action(&tool_name)),
            _ => description.map(|description| description.title),
        };
        match reason {
            ToolRejection::Unsupported => {}
            ToolRejection::MalformedArguments | ToolRejection::Invalid => {
                self.tool_calls
                    .push(ToolRecord::rejected(tool_name, arguments));
            }
            ToolRejection::Panicked => {
                self.tool_calls
                    .push(ToolRecord::preflight_failed(tool_name, arguments));
            }
        }
        title.map_or(Ok(()), |title| {
            let line = self.progress_line(&title);
            self.write_status(StatusBlock::Progress, &line)
        })
    }

    fn context_notice(&mut self, notice: &str) -> bool {
        if self.mode == OutputMode::Terminal
            || self.claimed_notices.iter().any(|seen| seen == notice)
        {
            return true;
        }
        self.claimed_notices.push(notice.to_owned());
        match write_stderr(&format!("[notice] {notice}\n")) {
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

    fn finish_command_output(&mut self, call_id: &ToolCallId) -> io::Result<()> {
        let Some(index) = self
            .command_calls
            .iter()
            .position(|command| command == call_id)
        else {
            return Ok(());
        };
        self.command_calls.remove(index);
        self.command_echo
            .as_ref()
            .map_or(Ok(()), |echo| echo.finish_line())
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
        self.held_blank_text.clear();
        if self.has_output {
            self.boundary_pending = true;
        }
    }

    fn push_assistant(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() || self.mode == OutputMode::Quiet {
            return Ok(());
        }
        if self.mode == OutputMode::Terminal {
            return self.push_terminal_text(text);
        }
        if self.boundary_pending {
            self.write_separator()?;
        }
        self.boundary_pending = false;
        self.write_output(text)
    }

    fn push_terminal_text(&mut self, text: &str) -> io::Result<()> {
        let visible = text.trim_end_matches(BLANK_TEXT);
        if visible.is_empty() {
            self.held_blank_text.push_str(text);
            return Ok(());
        }
        let mut pending = mem::replace(&mut self.held_blank_text, text[visible.len()..].to_owned());
        pending.push_str(visible);
        self.open_status_block = None;
        if !mem::take(&mut self.boundary_pending) {
            return self.write_output(&pending);
        }
        self.write_separator()?;
        self.write_output(without_leading_blank_lines(&pending))
    }

    fn write_status(&mut self, block: StatusBlock, line: &str) -> io::Result<()> {
        if self.mode != OutputMode::Terminal {
            return write_stderr(line);
        }
        self.held_blank_text.clear();
        if self.open_status_block == Some(block) {
            self.end_line()?;
        } else {
            self.write_separator()?;
            self.open_status_block = Some(block);
        }
        self.write_output(line)?;
        self.boundary_pending = true;
        Ok(())
    }

    fn write_separator(&mut self) -> io::Result<()> {
        if !self.has_output {
            return Ok(());
        }
        let separator = match self.trailing_newlines {
            0 => "\n\n",
            1 => "\n",
            _ => "",
        };
        self.write_output(separator)
    }

    fn end_line(&mut self) -> io::Result<()> {
        if self.has_output && self.trailing_newlines == 0 {
            self.write_output("\n")?;
        }
        Ok(())
    }

    fn write_output(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if self.mode == OutputMode::Json {
            self.output.push_str(text);
        } else {
            self.stdout.write_all(text.as_bytes())?;
            self.stdout.flush()?;
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
            TurnFailure::Provider(error) => {
                match ofx_gateway::http_failure(error, self.source.label()) {
                    Some(failure) => self.describe_http_failure(&failure),
                    None => self.describe_error(&error.code, error.detail.as_deref()),
                }
            }
            TurnFailure::InvalidCompletion
            | TurnFailure::ResponseLanguageMismatch
            | TurnFailure::MalformedProviderResult
            | TurnFailure::MalformedProviderArguments
            | TurnFailure::PermissionRequired(_)
            | TurnFailure::ProjectContext
            | TurnFailure::SkillContext(_)
            | TurnFailure::Compaction(_)
            | TurnFailure::Persistence(_)
            | TurnFailure::RecoveryPaused => self.describe_error(failure.code(), None),
            TurnFailure::StepLimitReached
            | TurnFailure::RepeatedMalformedArguments
            | TurnFailure::RepeatedShellExecutionFailure => Ok(FailureSummary {
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

    fn describe_http_failure(&mut self, failure: &HttpFailure<'_>) -> io::Result<FailureSummary> {
        write_stderr(&format!("oh-fx ask: {}\n", failure.message))?;
        if let Some(explanation) = failure.explanation {
            write_stderr(&format!("oh-fx ask: {explanation}\n"))?;
        }
        if let Some(guidance) = self.source.relogin().filter(|_| failure.unauthorized) {
            write_stderr(&format!("oh-fx ask: {guidance}\n"))?;
        }
        if self.mode == OutputMode::Json {
            self.output.push_str(&failure.message);
            self.output.push('\n');
        }
        Ok(FailureSummary {
            error: None,
            auth_failure: failure.unauthorized,
        })
    }

    fn finish(mut self, report: &TurnReport, model: &str, saved: Option<SavedAsk>) -> ExitCode {
        if self.mode == OutputMode::Terminal {
            let _ = self.end_line();
        }
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
        let untouched = summary.auth_failure && self.steps == 0;
        let durable = saved.is_some();
        let session_id = saved
            .map(|saved| saved.close(untouched))
            .unwrap_or_default();
        if self.mode != OutputMode::Json {
            if let Some(code) = self.write_error {
                let _ = write_stderr(&format!("oh-fx: {code}\n"));
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
            session_id: &session_id,
            steps: self.steps,
            tool_calls: &self.tool_calls,
            usage: usage_record(report.usage),
            error: summary.error.as_deref(),
            auth_failure: summary.auth_failure.then_some(AuthFailureRecord {
                source: self.source.label(),
                reason: "http_unauthorized",
                http_status: 401,
            }),
            recovery: self
                .recovery
                .as_ref()
                .map(|status| RecoveryRecord::new(status, durable)),
        })
    }

    fn finish_blocked(mut self, code: &str, blocked: &BlockedCall, usage: Usage) -> ExitCode {
        let error = match write_stderr(&self.blocked_action_guidance(&blocked.title)) {
            Ok(()) => code,
            Err(error) => write_error_name(&error),
        };
        self.tool_calls.push(ToolRecord::rejected(
            blocked.tool_name.clone(),
            &blocked.arguments,
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
    use std::fs;
    use std::io::{BufRead, BufReader, Read};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{self, Child, Stdio};
    use std::sync::{Mutex, mpsc};
    use std::time::{Duration, Instant};

    use ofx_auth::{ChatGptEndpoints, PreparationError};
    use ofx_cli::{CommandLaunch, Invocation};
    use ofx_config::ProfilePaths;
    use ofx_contract::{
        CallDescription, Concurrency, ModelFailureDiagnostic, ModelRecoveryRequiredAction,
        ProviderError, ProviderErrorKind, RouteRecoveryKind, ToolActivity, TurnId,
    };
    use ofx_gateway::CodexEndpoints;
    use ofx_testkit::{FakeServer, Reply};

    use super::*;

    const SIGNAL_CHILD_VARIABLE: &str = "OH_FX_TEST_STALLED_REFRESH_AUTH_URL";
    const SIGNAL_RECORDED: &str = "first signal recorded: ";
    const TITLE_CHILD_URL: &str = "OH_FX_TEST_TITLED_ASK_URL";
    const TITLE_CHILD_ARGS: &str = "OH_FX_TEST_TITLED_ASK_ARGS";
    const EXPIRED_SESSION: &str = r#"{"version":1,"access_token":"saved-access-token","refresh_token":"rt-refresh-secret-0123456789","expires_at_ms":1,"account_id":"acct_test"}
"#;
    const VALID_SESSION: &str = r#"{"version":1,"access_token":"eyJhbGciOiJub25lIn0.c2F2ZWQtYWNjZXNz.c2lnbmF0dXJl","refresh_token":"rt-refresh-secret-0123456789","expires_at_ms":4102444800000,"account_id":"acct_test"}
"#;

    struct ExpiredLogin {
        directory: tempfile::TempDir,
        paths: ProfilePaths,
        workspace: PathBuf,
    }

    impl ExpiredLogin {
        fn new() -> Self {
            Self::with_session(EXPIRED_SESSION)
        }

        fn with_session(saved: &str) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let paths = ProfilePaths {
                config: root.join("config/oh-fx"),
                data: root.join("data/oh-fx"),
                state: root.join("state/oh-fx"),
                cache: root.join("cache/oh-fx"),
            };
            let workspace = root.join("workspace");
            for directory in [&paths.config, &paths.data, &workspace] {
                fs::create_dir_all(directory).unwrap();
            }
            fs::set_permissions(&paths.data, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(
                paths.config.join("settings.json"),
                r#"{"provider":"codex","models":{"codex":"gpt-5.4"}}"#,
            )
            .unwrap();
            let session = paths.data.join("chatgpt-auth.json");
            fs::write(&session, saved).unwrap();
            fs::set_permissions(&session, fs::Permissions::from_mode(0o600)).unwrap();
            Self {
                directory,
                paths,
                workspace,
            }
        }

        fn root(&self) -> &Path {
            self.directory.path()
        }

        fn session(&self) -> String {
            fs::read_to_string(self.paths.data.join("chatgpt-auth.json")).unwrap()
        }

        fn settings(&self) -> Settings {
            Settings::load(&self.paths, &self.workspace).unwrap()
        }
    }

    fn endpoints(base_url: &str) -> SubscriptionEndpoints {
        SubscriptionEndpoints {
            chatgpt: ChatGptEndpoints {
                issuer: base_url.to_owned(),
                token_url: format!("{base_url}/oauth/token"),
                callback_ports: vec![0],
            },
            codex: CodexEndpoints {
                responses: format!("{base_url}/backend-api/codex/responses"),
            },
            ..SubscriptionEndpoints::default()
        }
    }

    struct Reaped(Child);

    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn send(child: &Child, name: &str) {
        let sent = process::Command::new("kill")
            .args([format!("-{name}"), child.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
    }

    fn assert_a_stalled_login_refresh_exits_by_the_first(signals: &[(&str, i32)]) {
        let login = ExpiredLogin::new();
        let auth = FakeServer::start([Reply::held_status_with_headers(200, &[], "")]);
        let root = login.root();
        let mut child = Reaped(
            process::Command::new(env::current_exe().unwrap())
                .args([
                    "cli_ask::tests::ask_child_with_a_stalled_login_refresh",
                    "--exact",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env_clear()
                .env("HOME", root)
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_STATE_HOME", root.join("state"))
                .env("XDG_CACHE_HOME", root.join("cache"))
                .env(SIGNAL_CHILD_VARIABLE, auth.base_url())
                .current_dir(&login.workspace)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdout = child.0.stdout.take().unwrap();
        let (line_sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line_sender.send(line).is_err() {
                    break;
                }
            }
        });
        let started = Instant::now();
        while auth.requests().is_empty() {
            if let Some(status) = child.0.try_wait().unwrap() {
                panic!("the child ended before refreshing the login: {status}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the child never asked to refresh the login"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let ((first_name, first), later) = signals.split_first().unwrap();
        let signalled = Instant::now();
        send(&child.0, first_name);
        let recorded = format!("{SIGNAL_RECORDED}{first}");
        loop {
            let remaining = Duration::from_secs(10).saturating_sub(signalled.elapsed());
            let line = lines
                .recv_timeout(remaining)
                .expect("the child records the first signal");
            if line.ends_with(&recorded) {
                break;
            }
        }
        for (name, _) in later {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "the child ended before the later signal"
            );
            send(&child.0, name);
        }
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                signalled.elapsed() < Duration::from_secs(10),
                "the child kept waiting for the stalled refresh"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.signal(), Some(*first));
        let mut stderr = String::new();
        child
            .0
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert_eq!(stderr, "");
        assert_eq!(auth.requests().len(), 1);
        assert_eq!(login.session(), EXPIRED_SESSION);
    }

    #[test]
    #[ignore = "run as a child process by the stalled login refresh signal tests"]
    fn ask_child_with_a_stalled_login_refresh() {
        let Ok(auth) = env::var(SIGNAL_CHILD_VARIABLE) else {
            return;
        };
        let Ok(Invocation::Command(CommandLaunch {
            command: ofx_cli::Command::Ask(args),
            ..
        })) = ofx_cli::parse_args(["ask", "Hello"])
        else {
            panic!("ask arguments");
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cancel = CancellationToken::new();
        let received = watch_signals(cancel.clone());
        let first = Arc::clone(&received.first);
        thread::spawn(move || {
            loop {
                match first.load(Ordering::SeqCst) {
                    0 => thread::sleep(Duration::from_millis(5)),
                    signal => break println!("{SIGNAL_RECORDED}{signal}"),
                }
            }
        });
        let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent"));
        let request = AskRequest {
            args: &args,
            prompt: "Hello",
            context_limits: &[],
            executions: &executions,
        };
        let answered =
            runtime.block_on(answer(&request, endpoints(&auth), None, &cancel, &received));
        let _ = settle(answered, &received);
    }

    #[test]
    fn an_interrupt_during_a_stalled_login_refresh_exits_by_that_signal() {
        assert_a_stalled_login_refresh_exits_by_the_first(&[("INT", SIGINT)]);
    }

    #[test]
    fn a_termination_during_a_stalled_login_refresh_exits_by_that_signal() {
        assert_a_stalled_login_refresh_exits_by_the_first(&[("TERM", SIGTERM)]);
    }

    #[test]
    fn a_termination_followed_by_an_interrupt_exits_by_the_termination() {
        assert_a_stalled_login_refresh_exits_by_the_first(&[("TERM", SIGTERM), ("INT", SIGINT)]);
    }

    fn codex_text(text: &str) -> Reply {
        let delta = serde_json::to_string(text).unwrap();
        Reply::sse(&[
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","phase":"final_answer"}}"#.to_owned(),
            format!(r#"{{"type":"response.output_text.delta","output_index":0,"delta":{delta}}}"#),
            r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":20,"output_tokens":3}}}"#.to_owned(),
        ])
    }

    fn titled_ask(login: &ExpiredLogin, codex: &FakeServer, args: &[&str]) -> process::Output {
        let root = login.root();
        process::Command::new(env::current_exe().unwrap())
            .args([
                "cli_ask::tests::ask_child_that_may_name_its_session",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("HOME", root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env(TITLE_CHILD_URL, codex.base_url())
            .env(TITLE_CHILD_ARGS, serde_json::to_string(args).unwrap())
            .current_dir(&login.workspace)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn saved_sessions(login: &ExpiredLogin) -> Vec<Value> {
        let Ok(entries) = fs::read_dir(login.paths.data.join("sessions")) else {
            return Vec::new();
        };
        entries
            .map(|entry| {
                let manifest = entry.unwrap().path().join("session.json");
                serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap()
            })
            .collect()
    }

    #[test]
    #[ignore = "run as a child process by the ask session title tests"]
    fn ask_child_that_may_name_its_session() {
        let (Ok(base_url), Ok(args)) = (env::var(TITLE_CHILD_URL), env::var(TITLE_CHILD_ARGS))
        else {
            return;
        };
        let words: Vec<String> = serde_json::from_str(&args).unwrap();
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        let args = ask_args(&words);
        let prompt = args.resolve_prompt(read_stdin_prompt).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cancel = CancellationToken::new();
        let received = watch_signals(cancel.clone());
        let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent"));
        let request = AskRequest {
            args: &args,
            prompt: &prompt,
            context_limits: &[],
            executions: &executions,
        };
        let answered = runtime.block_on(answer(
            &request,
            endpoints(&base_url),
            None,
            &cancel,
            &received,
        ));
        assert_eq!(settle(answered, &received), ExitCode::SUCCESS);
    }

    #[test]
    fn a_fresh_saved_ask_names_its_session_beside_the_first_turn() {
        let login = ExpiredLogin::with_session(VALID_SESSION);
        let codex = FakeServer::start([
            codex_text("Fix the renderer"),
            codex_text("Fix the renderer"),
        ]);
        let output = titled_ask(&login, &codex, &["ask", "please fix the renderer"]);
        assert!(output.status.success(), "{output:?}");
        let sessions = saved_sessions(&login);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["title"], "Fix the renderer");
        let requests = codex.requests();
        assert_eq!(requests.len(), 2);
        let titles: Vec<_> = requests
            .iter()
            .filter(|request| request.json()["model"] == "gpt-5.6-luna")
            .collect();
        assert_eq!(titles.len(), 1);
        assert_eq!(titles[0].json()["tool_choice"], "none");
        assert!(
            titles[0].json()["input"]
                .to_string()
                .contains("please fix the renderer")
        );
        assert_eq!(titles[0].header("session-id"), sessions[0]["id"].as_str());
    }

    #[test]
    fn an_unsaved_ask_or_one_with_titles_off_sends_no_title_request() {
        let login = ExpiredLogin::with_session(VALID_SESSION);
        let codex = FakeServer::start([codex_text("one"), codex_text("two")]);
        let unsaved = titled_ask(&login, &codex, &["ask", "--no-save", "fix the renderer"]);
        assert!(unsaved.status.success(), "{unsaved:?}");
        assert!(saved_sessions(&login).is_empty());
        fs::write(
            login.paths.config.join("settings.json"),
            r#"{"provider":"codex","models":{"codex":"gpt-5.4"},"session_titles":false}"#,
        )
        .unwrap();
        let disabled = titled_ask(&login, &codex, &["ask", "please fix the renderer"]);
        assert!(disabled.status.success(), "{disabled:?}");
        assert_eq!(codex.requests().len(), 2);
        assert_eq!(
            saved_sessions(&login)[0]["title"],
            "please fix the renderer"
        );
    }

    fn ask_args(args: &[&str]) -> AskArgs {
        let Ok(Invocation::Command(CommandLaunch {
            command: ofx_cli::Command::Ask(args),
            ..
        })) = ofx_cli::parse_args(args.iter().copied())
        else {
            panic!("ask arguments");
        };
        args
    }

    #[test]
    fn flags_override_the_saved_effort_and_fast_mode() {
        let login = ExpiredLogin::new();
        fs::write(
            login.paths.config.join("settings.json"),
            r#"{"provider":"codex","models":{"codex":"gpt-6.1-sol"},"effort":"high","fast_mode":true,"fast_mode_model_bound":true}"#,
        )
        .unwrap();
        let saved = login.settings();
        let cases: [(&[&str], Option<&str>, Option<bool>); 5] = [
            (&["ask", "hi"], Some("high"), None),
            (
                &["ask", "--effort", "low", "--no-fast", "hi"],
                Some("low"),
                Some(false),
            ),
            (&["ask", "--effort", "auto", "hi"], None, None),
            (
                &["ask", "--model", "gpt-5.6-terra", "hi"],
                Some("high"),
                None,
            ),
            (
                &["ask", "--model", "gpt-5.6-terra", "--fast", "hi"],
                Some("high"),
                Some(true),
            ),
        ];
        for (args, effort, fast) in cases {
            assert_eq!(
                requested_reasoning(&ask_args(args), &saved, None),
                (effort.map(str::to_owned), fast),
                "{args:?}"
            );
        }
        assert_eq!(
            requested_reasoning(
                &ask_args(&["ask", "--fast", "hi"]),
                &Settings::default(),
                None
            ),
            (None, Some(true))
        );
        assert_eq!(
            requested_reasoning(&ask_args(&["ask", "hi"]), &Settings::default(), None),
            (None, None)
        );
        let resumed = SessionPreferences {
            provider: ofx_session::SavedProvider::new(ofx_config::ProviderId::Codex, None).unwrap(),
            model: "gpt-5.4".to_owned(),
            effort: ofx_contract::ReasoningEffort::parse("medium").unwrap(),
            fast_mode: false,
        };
        assert_eq!(
            requested_reasoning(&ask_args(&["ask", "hi"]), &saved, Some(&resumed)),
            (Some("medium".to_owned()), Some(false))
        );
        assert_eq!(
            requested_reasoning(
                &ask_args(&["ask", "--effort", "low", "--fast", "hi"]),
                &saved,
                Some(&resumed)
            ),
            (Some("low".to_owned()), Some(true))
        );
    }

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
            ToolRecord::new("read_file".to_owned(), "{}", ToolResultStatus::Success),
            ToolRecord::new("read_file".to_owned(), "{}", ToolResultStatus::Failure),
        ];
        let recovered = RouteRecoveryStatus {
            kind: RouteRecoveryKind::AutoRecovered,
            failed_attempt: 0,
            succeeded_attempt: 2,
            attempt_limit: 10,
            cause: None,
            action: None,
            required_action: ModelRecoveryRequiredAction::None,
            delay_seconds: 0,
            diagnostic: None,
            retry_wait: None,
        };
        let result = RunResult {
            tool_calls: &records,
            error: None,
            recovery: Some(RecoveryRecord::new(&recovered, false)),
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
            required_action: ModelRecoveryRequiredAction::None,
            delay_seconds: 2,
            diagnostic: Some(ModelFailureDiagnostic::new("HTTP 429 · slow")),
            retry_wait: None,
        };
        assert_eq!(
            serde_json::to_string(&RecoveryRecord::new(&retrying, true)).unwrap(),
            r#"{"state":"active","kind":"auto_retry","cause":"rate_limited","action":"retrying_request","attempt":2,"attempt_limit":10,"delay_seconds":2,"durable":true,"message":"⚠ Rate limited · HTTP 429 · slow · retrying request in 2s"}"#
        );
    }

    #[test]
    fn a_stopped_recovery_is_reported_in_every_mode_and_names_an_unsaved_run() {
        let stopped = RouteRecoveryStatus {
            kind: RouteRecoveryKind::TerminalProviderError,
            failed_attempt: 2,
            succeeded_attempt: 0,
            attempt_limit: 10,
            cause: Some(ModelRecoveryCause::ProviderUnavailable),
            action: None,
            required_action: ModelRecoveryRequiredAction::None,
            delay_seconds: 0,
            diagnostic: Some(ModelFailureDiagnostic::new("ConnectionFailed")),
            retry_wait: None,
        };
        let retrying = RouteRecoveryStatus {
            kind: RouteRecoveryKind::AutoRetry,
            action: Some(ModelRecoveryAction::RetryingRequest),
            ..stopped.clone()
        };
        let label =
            "[notice] ⚠ Provider unavailable · ConnectionFailed · stopped after 2 attempts\n";
        let unsaved = "[notice] This run was started with --no-save, so its recovery context cannot be resumed after exit.\n";
        let mut presenter = json_presenter();
        presenter.mode = OutputMode::Quiet;
        assert!(presenter.recovery_notices(&retrying).is_empty());
        assert_eq!(presenter.recovery_notices(&stopped), [label, unsaved]);
        let mut presenter = json_presenter().saving(true);
        assert_eq!(presenter.recovery_notices(&stopped), [label]);
        presenter.mode = OutputMode::Terminal;
        assert_eq!(
            presenter.recovery_notices(&retrying),
            ["[notice] ⚠ Provider unavailable · ConnectionFailed · retrying request\n"]
        );
        assert_eq!(
            serde_json::to_string(&RecoveryRecord::new(&stopped, false)).unwrap(),
            r#"{"state":"failed","kind":"terminal_provider_error","cause":"provider_unavailable","attempt":2,"attempt_limit":10,"delay_seconds":0,"durable":false,"message":"⚠ Provider unavailable · ConnectionFailed · stopped after 2 attempts"}"#
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
    fn terminal_runs_keep_context_notices_out_of_the_transcript() {
        let mut presenter = json_presenter();
        presenter.mode = OutputMode::Terminal;
        assert!(presenter.context_notice("[context] hidden"));
        assert!(presenter.claimed_notices.is_empty());
        assert!(!presenter.has_output);
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

    fn started(call_id: &str, title: &str, effect: ToolEffect) -> UiEvent {
        UiEvent::ToolStarted {
            turn_id: TurnId::new(1),
            call_id: ToolCallId::new(call_id),
            tool_name: "read_file".to_owned(),
            description: CallDescription {
                title: title.to_owned(),
                label: None,
                activity: ToolActivity::Read,
                effect,
                concurrency: Concurrency::Parallel,
            },
        }
    }

    fn finished(call_id: &str) -> UiEvent {
        UiEvent::ToolFinished {
            turn_id: TurnId::new(1),
            call_id: ToolCallId::new(call_id),
            tool_name: "read_file".to_owned(),
            arguments: "{}".to_owned(),
            status: ToolResultStatus::Success,
            content: String::new(),
            command_result: None,
            process: None,
            status_detail: None,
            file_change: None,
        }
    }

    fn rejected(
        call_id: &str,
        tool_name: &str,
        arguments: &str,
        reason: ToolRejection,
        title: Option<&str>,
    ) -> UiEvent {
        UiEvent::ToolRejected {
            turn_id: TurnId::new(1),
            call_id: ToolCallId::new(call_id),
            tool_name: tool_name.to_owned(),
            arguments: arguments.to_owned(),
            reason,
            description: title
                .filter(|_| reason != ToolRejection::Unsupported)
                .map(|title| CallDescription {
                    title: title.to_owned(),
                    label: None,
                    activity: ToolActivity::Read,
                    effect: ToolEffect::None,
                    concurrency: Concurrency::Parallel,
                }),
            content: String::new(),
        }
    }

    fn assistant(text: &str) -> UiEvent {
        UiEvent::AssistantText {
            turn_id: TurnId::new(1),
            text: text.to_owned(),
        }
    }

    fn present(presenter: &mut Presenter, events: impl IntoIterator<Item = UiEvent>) {
        for event in events {
            assert!(presenter.handle(event));
        }
    }

    fn report(failure: Option<TurnFailure>) -> TurnReport {
        TurnReport {
            outcome: if failure.is_some() {
                TurnOutcome::Failed
            } else {
                TurnOutcome::Completed
            },
            final_text: String::new(),
            usage: Usage::default(),
            failure,
        }
    }

    #[derive(Clone, Default)]
    struct Screen(Arc<Mutex<Vec<u8>>>);

    impl Screen {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for Screen {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn terminal_presenter() -> (Presenter, Screen) {
        let screen = Screen::default();
        let mut presenter = json_presenter();
        presenter.mode = OutputMode::Terminal;
        presenter.stdout = Box::new(screen.clone());
        (presenter, screen)
    }

    #[test]
    fn raw_output_separates_text_around_tool_steps_and_counts_rejections() {
        let mut presenter = json_presenter();
        presenter.push_assistant("Looking.").unwrap();
        assert!(presenter.handle(started("call-1", "Reading", ToolEffect::ReadOnly)));
        presenter.push_assistant("Found it.").unwrap();
        assert!(presenter.handle(rejected(
            "call-2",
            "missing",
            "{}",
            ToolRejection::Unsupported,
            Some("Working: missing")
        )));
        presenter.push_assistant("\nDone").unwrap();
        assert_eq!(presenter.output, "Looking.\n\nFound it.\n\n\nDone");
        assert_eq!(presenter.steps, 2);
        assert!(presenter.tool_calls.is_empty());
    }

    #[test]
    fn calls_that_never_run_are_recorded_as_upstream_records_them() {
        let mut presenter = json_presenter();
        let run = r#"{"request":{"action":"run"}}"#;
        let stop = r#"{"action":"stop","session_id":"shell-1"}"#;
        present(
            &mut presenter,
            [
                rejected(
                    "call-1",
                    "missing",
                    run,
                    ToolRejection::Unsupported,
                    Some("Working: missing"),
                ),
                rejected(
                    "call-2",
                    "shell",
                    run,
                    ToolRejection::Invalid,
                    Some("Running command"),
                ),
                rejected("call-3", "read_file", "{}", ToolRejection::Invalid, None),
                rejected("call-4", "shell", stop, ToolRejection::Panicked, None),
                rejected("call-5", "read_file", "{}", ToolRejection::Panicked, None),
            ],
        );
        assert_eq!(presenter.steps, 5);
        assert_eq!(
            serde_json::to_string(&presenter.tool_calls).unwrap(),
            concat!(
                r#"[{"name":"shell","status":"error","action":"run","error":{"category":"rejected","code":"rejected"}},"#,
                r#"{"name":"read_file","status":"error"},"#,
                r#"{"name":"shell","status":"error","action":"stop","error":{"category":"tool_failed","code":"tool_failed"}},"#,
                r#"{"name":"read_file","status":"error"}]"#,
            )
        );
    }

    #[test]
    fn calls_with_malformed_arguments_are_recorded_as_upstream_records_them() {
        let mut presenter = json_presenter();
        present(
            &mut presenter,
            [
                rejected(
                    "call-1",
                    "shell",
                    "{}",
                    ToolRejection::MalformedArguments,
                    None,
                ),
                rejected(
                    "call-2",
                    "write_file",
                    "{}",
                    ToolRejection::MalformedArguments,
                    None,
                ),
            ],
        );
        assert_eq!(presenter.steps, 2);
        assert_eq!(
            serde_json::to_string(&presenter.tool_calls).unwrap(),
            concat!(
                r#"[{"name":"shell","status":"error","error":{"category":"rejected","code":"rejected"}},"#,
                r#"{"name":"write_file","status":"error"}]"#,
            )
        );
        for silent in [
            TurnFailure::RepeatedMalformedArguments,
            TurnFailure::RepeatedShellExecutionFailure,
        ] {
            let summary = presenter.describe_failure(&silent);
            assert_eq!(summary.error, None);
            assert!(!summary.auth_failure);
        }
    }

    #[test]
    fn terminal_output_shows_rejected_calls_with_titles_as_safe_progress_lines() {
        let (mut presenter, screen) = terminal_presenter();
        present(
            &mut presenter,
            [
                assistant("Looking."),
                rejected(
                    "call-1",
                    "missing\x1b[2J",
                    "{}",
                    ToolRejection::Unsupported,
                    Some("Working: missing\x1b[2J"),
                ),
                rejected(
                    "call-2",
                    "shell",
                    r#"{"action":"run"}"#,
                    ToolRejection::Invalid,
                    Some("Running command"),
                ),
                rejected("call-3", "read_file", "{}", ToolRejection::Invalid, None),
                assistant("Done."),
            ],
        );
        assert_eq!(
            screen.text(),
            "Looking.\n\nWorking: missing\\x1b[2J\nRunning command\n\nDone."
        );
        assert_eq!(presenter.steps, 3);
    }

    #[test]
    fn terminal_output_puts_tool_progress_on_its_own_line_between_blank_lines() {
        let (mut presenter, screen) = terminal_presenter();
        present(
            &mut presenter,
            [
                assistant("I'll read `readme.md` to find the project name."),
                started("call-1", "Reading readme.md", ToolEffect::ReadOnly),
                finished("call-1"),
                assistant(
                    "The project is named **oh-fx**, as shown in the heading of `README.md`.",
                ),
            ],
        );
        assert_eq!(
            presenter.finish(&report(None), "m", None),
            ExitCode::SUCCESS
        );
        assert_eq!(
            screen.text(),
            "I'll read `readme.md` to find the project name.\n\nReading readme.md\n\nThe project is named **oh-fx**, as shown in the heading of `README.md`.\n"
        );
    }

    #[test]
    fn terminal_output_leaves_one_blank_line_whatever_newlines_end_the_text() {
        for looking in ["Looking.", "Looking.\n", "Looking.\n\n"] {
            let (mut presenter, screen) = terminal_presenter();
            present(
                &mut presenter,
                [
                    assistant(looking),
                    started("call-1", "Reading a.txt", ToolEffect::ReadOnly),
                    finished("call-1"),
                    assistant("Found it."),
                ],
            );
            assert_eq!(
                screen.text(),
                "Looking.\n\nReading a.txt\n\nFound it.",
                "{looking:?}"
            );
        }
    }

    #[test]
    fn terminal_output_keeps_consecutive_tool_progress_lines_together() {
        let (mut presenter, screen) = terminal_presenter();
        present(
            &mut presenter,
            [
                started("call-1", "Reading a.txt", ToolEffect::ReadOnly),
                started("call-2", "Reading b.txt", ToolEffect::ReadOnly),
                finished("call-1"),
                finished("call-2"),
                started("call-3", "Reading c.txt", ToolEffect::ReadOnly),
                finished("call-3"),
                assistant("Read three files."),
                started("call-4", "Reading d.txt", ToolEffect::ReadOnly),
                finished("call-4"),
                assistant("And one more."),
            ],
        );
        assert_eq!(
            screen.text(),
            "Reading a.txt\nReading b.txt\nReading c.txt\n\nRead three files.\n\nReading d.txt\n\nAnd one more."
        );
        assert_eq!(presenter.steps, 4);
    }

    #[test]
    fn terminal_output_shows_a_settling_tool_in_its_block_when_it_finishes() {
        let (mut presenter, screen) = terminal_presenter();
        present(
            &mut presenter,
            [
                assistant("Checking."),
                started("call-1", "Reading a.txt", ToolEffect::None),
            ],
        );
        assert_eq!(screen.text(), "Checking.");
        present(
            &mut presenter,
            [started("call-2", "Reading b.txt", ToolEffect::ReadOnly)],
        );
        assert_eq!(screen.text(), "Checking.\n\nReading b.txt\n");
        present(
            &mut presenter,
            [
                finished("call-2"),
                finished("call-1"),
                assistant("Done checking."),
            ],
        );
        assert_eq!(
            screen.text(),
            "Checking.\n\nReading b.txt\nReading a.txt\n\nDone checking."
        );

        let (mut presenter, screen) = terminal_presenter();
        present(
            &mut presenter,
            [
                assistant("Checking."),
                started("call-1", "Reading a.txt", ToolEffect::None),
                finished("call-1"),
                assistant("Done checking."),
            ],
        );
        assert_eq!(
            screen.text(),
            "Checking.\n\nReading a.txt\n\nDone checking."
        );
    }

    #[test]
    fn terminal_output_groups_recovery_notices_apart_from_tool_progress() {
        let retrying = RouteRecoveryStatus {
            kind: RouteRecoveryKind::AutoRetry,
            failed_attempt: 1,
            succeeded_attempt: 0,
            attempt_limit: 10,
            cause: Some(ModelRecoveryCause::RateLimited),
            action: Some(ModelRecoveryAction::RetryingRequest),
            required_action: ModelRecoveryRequiredAction::None,
            delay_seconds: 2,
            diagnostic: None,
            retry_wait: None,
        };
        let recovered = RouteRecoveryStatus {
            kind: RouteRecoveryKind::AutoRecovered,
            failed_attempt: 0,
            succeeded_attempt: 2,
            cause: None,
            action: None,
            required_action: ModelRecoveryRequiredAction::None,
            delay_seconds: 0,
            ..retrying.clone()
        };
        let notice = |status: &RouteRecoveryStatus| UiEvent::Recovery {
            turn_id: TurnId::new(1),
            status: status.clone(),
        };
        let (mut presenter, screen) = terminal_presenter();
        present(
            &mut presenter,
            [
                assistant("Looking."),
                started("call-1", "Reading a.txt", ToolEffect::ReadOnly),
                finished("call-1"),
                notice(&retrying),
                notice(&recovered),
                started("call-2", "Reading b.txt", ToolEffect::ReadOnly),
                finished("call-2"),
                assistant("Found it."),
            ],
        );
        assert_eq!(
            screen.text(),
            format!(
                "Looking.\n\nReading a.txt\n\n[notice] {}\n[notice] {}\n\nReading b.txt\n\nFound it.",
                retrying.label(),
                recovered.label()
            )
        );
    }

    #[test]
    fn terminal_output_shows_system_notices_as_notice_lines_after_tool_progress() {
        let (mut presenter, screen) = terminal_presenter();
        present(
            &mut presenter,
            [
                started("call-1", "Running command", ToolEffect::ReadOnly),
                finished("call-1"),
                UiEvent::SystemNotice {
                    text: "Repeated shell validation failures stopped the tool loop.".to_owned(),
                },
            ],
        );
        assert_eq!(
            screen.text(),
            "Running command\n\n[notice] Repeated shell validation failures stopped the tool loop.\n"
        );
    }

    fn operational(text: &str) -> UiEvent {
        UiEvent::Operational {
            turn_id: TurnId::new(1),
            text: text.to_owned(),
        }
    }

    #[test]
    fn terminal_output_shows_done_alone_for_a_blank_reply() {
        for reply in [&["  "][..], &["  ", "\n"]] {
            let (mut presenter, screen) = terminal_presenter();
            present(&mut presenter, reply.iter().map(|text| assistant(text)));
            present(&mut presenter, [operational("Done.")]);
            assert_eq!(
                presenter.finish(&report(None), "m", None),
                ExitCode::SUCCESS
            );
            assert_eq!(screen.text(), "Done.\n", "{reply:?}");
        }

        let (mut presenter, screen) = terminal_presenter();
        present(
            &mut presenter,
            [
                assistant("Looking."),
                started("call-1", "Reading a.txt", ToolEffect::ReadOnly),
                finished("call-1"),
                assistant("  \n"),
                operational("Done."),
            ],
        );
        assert_eq!(
            presenter.finish(&report(None), "m", None),
            ExitCode::SUCCESS
        );
        assert_eq!(screen.text(), "Looking.\n\nReading a.txt\n\nDone.\n");
    }

    fn read(call_id: &str) -> [UiEvent; 2] {
        [
            started(call_id, "Reading a.txt", ToolEffect::ReadOnly),
            finished(call_id),
        ]
    }

    #[test]
    fn terminal_output_merges_blank_text_with_the_separator_around_a_tool() {
        let cases: [(&[&str], &[&str], &str); 4] = [
            (
                &["Looking."],
                &["\n", "Found it."],
                "Looking.\n\nReading a.txt\n\nFound it.\n",
            ),
            (
                &["Looking."],
                &["\n\n  Found", " it.\n\n"],
                "Looking.\n\nReading a.txt\n\n  Found it.\n",
            ),
            (
                &["Looking.\n\n\n"],
                &["Found it."],
                "Looking.\n\nReading a.txt\n\nFound it.\n",
            ),
            (
                &["Looking.", "  \n"],
                &["  \n", "\n", "Found", "\n\n", "it.\n"],
                "Looking.\n\nReading a.txt\n\nFound\n\nit.\n",
            ),
        ];
        for (before, after, expected) in cases {
            let (mut presenter, screen) = terminal_presenter();
            present(&mut presenter, before.iter().map(|text| assistant(text)));
            present(&mut presenter, read("call-1"));
            present(&mut presenter, after.iter().map(|text| assistant(text)));
            assert_eq!(
                presenter.finish(&report(None), "m", None),
                ExitCode::SUCCESS
            );
            assert_eq!(screen.text(), expected, "{before:?} {after:?}");
        }
    }

    #[test]
    fn terminal_output_keeps_blank_text_that_opens_the_output() {
        let (mut presenter, screen) = terminal_presenter();
        present(&mut presenter, [assistant("\n\n"), assistant("  Hello")]);
        assert_eq!(screen.text(), "\n\n  Hello");

        let (mut presenter, screen) = terminal_presenter();
        present(
            &mut presenter,
            [assistant("Looking.")]
                .into_iter()
                .chain(read("call-1"))
                .chain([assistant("\n")])
                .chain(read("call-2"))
                .chain([assistant("Found it.")]),
        );
        assert_eq!(
            screen.text(),
            "Looking.\n\nReading a.txt\nReading a.txt\n\nFound it."
        );
    }

    fn sequence_events(piece: usize, index: usize) -> Vec<UiEvent> {
        let call_id = format!("call-{index}");
        match piece {
            0 => read(&call_id).to_vec(),
            1 => vec![
                started(&call_id, "Reading b.txt", ToolEffect::None),
                finished(&call_id),
            ],
            2 => vec![rejected(
                &call_id,
                "missing",
                "{}",
                ToolRejection::Unsupported,
                Some("Working: missing"),
            )],
            3 => vec![UiEvent::Recovery {
                turn_id: TurnId::new(1),
                status: RouteRecoveryStatus {
                    kind: RouteRecoveryKind::AutoRecovered,
                    failed_attempt: 0,
                    succeeded_attempt: 2,
                    attempt_limit: 10,
                    cause: None,
                    action: None,
                    required_action: ModelRecoveryRequiredAction::None,
                    delay_seconds: 0,
                    diagnostic: None,
                    retry_wait: None,
                },
            }],
            4 => vec![operational("Done.")],
            _ => vec![assistant(
                ["\n", "  ", "Text", "Text\n\n\n", "\n\nText", "  \n  Text  "][piece - 5],
            )],
        }
    }

    fn status_kind(line: &str) -> Option<StatusBlock> {
        match line {
            "Reading a.txt" | "Reading b.txt" | "Working: missing" => Some(StatusBlock::Progress),
            "Done." => Some(StatusBlock::Operational),
            line if line.starts_with("[notice] ") => Some(StatusBlock::Notice),
            _ => None,
        }
    }

    fn assert_status_lines_stand_apart(screen: &str, label: &str) {
        assert!(!screen.ends_with("\n\n"), "{label}: {screen:?}");
        assert!(
            screen.is_empty() || screen.ends_with('\n'),
            "{label}: {screen:?}"
        );
        let mut previous: Option<(Option<StatusBlock>, usize)> = None;
        let mut blank_lines = 0;
        for line in screen.lines() {
            if line.trim_matches(BLANK_TEXT).is_empty() {
                blank_lines += 1;
                continue;
            }
            let kind = status_kind(line);
            assert!(
                kind.is_some()
                    || !["Reading", "Working", "Done.", "[notice]"]
                        .iter()
                        .any(|status| line.contains(status)),
                "{label}: a status line shares a line with text in {screen:?}"
            );
            let expected = match previous {
                None => kind.map(|_| 0),
                Some((previous_kind, _)) if previous_kind.is_none() && kind.is_none() => None,
                Some((previous_kind, _)) => Some(usize::from(previous_kind != kind)),
            };
            if let Some(expected) = expected {
                assert_eq!(blank_lines, expected, "{label}: {screen:?}");
            }
            previous = Some((kind, blank_lines));
            blank_lines = 0;
        }
    }

    #[test]
    fn terminal_output_never_glues_or_doubles_blank_lines_around_status_lines() {
        let pieces: usize = 11;
        let length = 4;
        for mut code in 0..pieces.pow(length) {
            let mut sequence = Vec::new();
            for _ in 0..length {
                sequence.push(code % pieces);
                code /= pieces;
            }
            let (mut presenter, screen) = terminal_presenter();
            for (index, piece) in sequence.iter().enumerate() {
                present(&mut presenter, sequence_events(*piece, index));
            }
            let _ = presenter.finish(&report(None), "m", None);
            assert_status_lines_stand_apart(&screen.text(), &format!("{sequence:?}"));
        }
    }

    #[test]
    fn terminal_output_shows_operational_text_and_ends_the_line_before_failures() {
        let (mut presenter, screen) = terminal_presenter();
        present(
            &mut presenter,
            [
                assistant("Looking."),
                started("call-1", "Reading a.txt", ToolEffect::ReadOnly),
                finished("call-1"),
                operational("Step limit reached.\n"),
            ],
        );
        let failure = Some(TurnFailure::StepLimitReached);
        assert_eq!(
            presenter.finish(&report(failure), "m", None),
            ExitCode::FAILURE
        );
        assert_eq!(
            screen.text(),
            "Looking.\n\nReading a.txt\n\nStep limit reached.\n"
        );

        let (mut presenter, screen) = terminal_presenter();
        present(&mut presenter, [assistant("Partial answer")]);
        let failure = Some(TurnFailure::StepLimitReached);
        assert_eq!(
            presenter.finish(&report(failure), "m", None),
            ExitCode::FAILURE
        );
        assert_eq!(screen.text(), "Partial answer\n");

        let notice = "Repeated malformed tool arguments stopped the agent loop. The invalid calls were not executed. Continue with a follow-up prompt if needed.";
        let (mut presenter, screen) = terminal_presenter();
        present(&mut presenter, [operational(&format!("{notice}\n"))]);
        let failure = Some(TurnFailure::RepeatedMalformedArguments);
        assert_eq!(
            presenter.finish(&report(failure), "m", None),
            ExitCode::FAILURE
        );
        assert_eq!(screen.text(), format!("{notice}\n"));
    }
}
