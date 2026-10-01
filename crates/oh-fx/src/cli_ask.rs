use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use ofx_agent::{
    Agent, AgentConfig, TurnFailure, TurnReport, normalize_assistant_text_for_display,
    text_for_completed_presentation,
};
use ofx_config::{ConnectionError, ProfilePaths, SelectionError, Settings, request_output_tokens};
use ofx_contract::{
    ModelRecoveryAction, ModelRecoveryCause, PermissionMode, ProviderError, RouteRecoveryStatus,
    ToolResultStatus, TurnOutcome, UiEvent, Usage,
};
use ofx_gateway::ChatCompletionsProvider;
use serde::Serialize;
use signal_hook::consts::{SIGINT, SIGTERM};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::cli::{ASK_USAGE, AskArgsError, AskArguments, AskOptions};
use crate::context::{GATEWAY_SYSTEM_PROMPT, HostRuntimeContext};

const STDIN_PROMPT_LIMIT: u64 = 8 * 1024 * 1024;
const CONFIGURED_SOURCE_LABEL: &str = "configured provider";
const YOLO_WARNING: &str = "Full access enabled: oh-fx permission checks disabled";
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];

struct Failure {
    code: String,
    notice: Option<String>,
    notice_in_json: bool,
    usage: bool,
}

impl Failure {
    fn code(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            notice: None,
            notice_in_json: false,
            usage: false,
        }
    }

    fn notice(code: impl Into<String>, notice: impl Into<String>) -> Self {
        Self {
            notice: Some(notice.into()),
            ..Self::code(code)
        }
    }

    fn with_usage(mut self) -> Self {
        self.usage = true;
        self
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
        } else if !json && !self.usage {
            let _ = writeln!(stderr, "oh-fx: {}", self.code);
        }
        if self.usage && !json {
            let _ = writeln!(stderr, "{ASK_USAGE}");
        }
        drop(stderr);
        if json {
            return print_result(&RunResult::error(&self.code));
        }
        ExitCode::FAILURE
    }
}

impl From<SelectionError> for Failure {
    fn from(error: SelectionError) -> Self {
        match error {
            SelectionError::ModelNotSelected | SelectionError::ProviderUnavailable(_) => {
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

pub(crate) fn run(arguments: &AskArguments) -> ExitCode {
    let options_json = arguments.requests_json();
    let options = match arguments.options() {
        Ok(options) => options,
        Err(AskArgsError::InvalidAskArgs) => {
            return Failure::code("InvalidAskArgs")
                .with_usage()
                .report(options_json);
        }
        Err(AskArgsError::InvalidPromptText) => return invalid_prompt_text().report(options_json),
    };
    let prompt = match read_prompt(options.prompt.as_deref()) {
        Ok(prompt) => prompt,
        Err(failure) => return failure.report(options_json),
    };
    if prompt.is_empty() {
        return Failure::code("InvalidConversationEvent").report(options.json);
    }
    crate::auto_upgrade::announce_and_schedule();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    match runtime {
        Ok(runtime) => runtime.block_on(ask(&options, &prompt)),
        Err(_) => Failure::code("RuntimeUnavailable").report(options.json),
    }
}

fn invalid_prompt_text() -> Failure {
    Failure::notice(
        "InvalidPromptText",
        "prompt must be valid UTF-8 and contain no NUL bytes",
    )
}

fn read_prompt(words: Option<&str>) -> Result<String, Failure> {
    let prompt = match words {
        Some(prompt) => prompt.to_owned(),
        None => read_stdin_prompt()?,
    };
    if prompt.contains('\0') {
        return Err(invalid_prompt_text());
    }
    Ok(prompt)
}

fn read_stdin_prompt() -> Result<String, Failure> {
    let missing = || Failure::notice("MissingPrompt", "missing prompt").with_usage();
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return Err(missing());
    }
    let mut bytes = Vec::new();
    stdin
        .lock()
        .take(STDIN_PROMPT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            Failure::notice("PromptInputReadFailed", "failed to read prompt from stdin")
        })?;
    if bytes.len() as u64 > STDIN_PROMPT_LIMIT {
        return Err(Failure::notice(
            "PromptResourceLimitExceeded",
            "prompt exceeds the local input safety limit",
        ));
    }
    let text = String::from_utf8(bytes).map_err(|_| invalid_prompt_text())?;
    let trimmed = text.trim_matches(TRIMMED);
    if trimmed.is_empty() {
        return Err(missing());
    }
    Ok(trimmed.to_owned())
}

async fn ask(options: &AskOptions, prompt: &str) -> ExitCode {
    let cancel = CancellationToken::new();
    let received_signal = watch_signals(cancel.clone());
    let (mut agent, model) = match prepare_agent(options) {
        Ok(prepared) => prepared,
        Err(failure) => return failure.report(options.json),
    };
    let mut presenter = Presenter::new(options.json);
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

fn prepare_agent(options: &AskOptions) -> Result<(Agent, String), Failure> {
    let workspace_root = workspace_root()?;
    let settings = match ProfilePaths::from_environment() {
        Some(paths) => Settings::load(&paths, &workspace_root)
            .map_err(|error| Failure::code(error.to_string()))?,
        None => Settings::default(),
    };
    if settings.profile_is_unusable() {
        return Err(Failure::code("InvalidProfileConfiguration"));
    }
    let permission_mode = settings.permission_mode();
    let mut stderr = io::stderr().lock();
    if permission_mode == PermissionMode::Yolo && !settings.yolo_acknowledged() {
        let warning = if stderr.is_terminal() {
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
    let connection = settings.selected_connection(&lookup)?;
    let model = settings.selected_model(connection, options.model.as_deref(), &lookup)?;
    let resolved = connection.resolve(&lookup, env::home_dir().as_deref())?;
    let provider = ChatCompletionsProvider::new(resolved, &crate::user_agent())
        .map_err(|error| Failure::notice("InvalidConnection", error.to_string()))?;
    let config = AgentConfig {
        system_prompt: GATEWAY_SYSTEM_PROMPT.to_owned(),
        max_output_tokens: request_output_tokens(connection.capabilities(&model)),
        step_limit: settings.max_agent_steps(&lookup),
        model: model.clone(),
    };
    let context = HostRuntimeContext::new(workspace_root, permission_mode);
    let agent = Agent::new(Arc::new(provider), Vec::new(), Arc::new(context), config);
    Ok((agent, model))
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
    if error.kind() == io::ErrorKind::BrokenPipe {
        "BrokenPipe"
    } else {
        "WriteFailed"
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
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ToolRecordError>,
}

#[derive(Serialize)]
struct ToolRecordError {
    category: &'static str,
    code: &'static str,
}

impl ToolRecord {
    fn new(name: String, status: ToolResultStatus) -> Self {
        let error = (status == ToolResultStatus::Failure).then_some(ToolRecordError {
            category: "tool_failed",
            code: "tool_failed",
        });
        Self {
            name,
            status: if error.is_some() { "error" } else { "success" },
            error,
        }
    }

    fn rejected(name: String) -> Self {
        Self {
            name,
            status: "error",
            error: Some(ToolRecordError {
                category: "rejected",
                code: "rejected",
            }),
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

#[derive(Serialize)]
struct RunResult<'a> {
    output: &'a str,
    final_output: &'a str,
    exit_code: u8,
    model: &'a str,
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
            model: "",
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
    Raw,
    Terminal,
}

struct Presenter {
    mode: OutputMode,
    output: String,
    has_output: bool,
    boundary_pending: bool,
    trailing_newlines: usize,
    steps: u64,
    tool_calls: Vec<ToolRecord>,
    recovery: Option<RouteRecoveryStatus>,
    write_error: Option<&'static str>,
}

struct FailureSummary {
    error: Option<String>,
    auth_failure: bool,
}

impl Presenter {
    fn new(json: bool) -> Self {
        let mode = if json {
            OutputMode::Json
        } else if io::stdout().is_terminal() {
            OutputMode::Terminal
        } else {
            OutputMode::Raw
        };
        Self {
            mode,
            output: String::new(),
            has_output: false,
            boundary_pending: false,
            trailing_newlines: 0,
            steps: 0,
            tool_calls: Vec::new(),
            recovery: None,
            write_error: None,
        }
    }

    fn handle(&mut self, event: UiEvent) -> bool {
        let written = match event {
            UiEvent::AssistantText { text, .. } => {
                self.push_assistant(&text);
                Ok(())
            }
            UiEvent::Operational { text, .. } => write_stderr(&text),
            UiEvent::Recovery { status, .. } => {
                let line = format!("[notice] {}\n", status.label());
                self.recovery = Some(status);
                write_stderr(&line)
            }
            UiEvent::ToolStarted { .. } => {
                self.start_step();
                Ok(())
            }
            UiEvent::ToolFinished {
                tool_name, status, ..
            } => {
                self.tool_calls.push(ToolRecord::new(tool_name, status));
                Ok(())
            }
            UiEvent::ToolRejected { tool_name, .. } => {
                self.start_step();
                self.tool_calls.push(ToolRecord::rejected(tool_name));
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

    fn start_step(&mut self) {
        self.steps += 1;
        if self.has_output {
            self.boundary_pending = true;
        }
    }

    fn push_assistant(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.boundary_pending && self.has_output {
            let separator = match self.trailing_newlines {
                0 => "\n\n",
                1 => "\n",
                _ => "",
            };
            self.write_assistant(separator);
        }
        self.boundary_pending = false;
        self.write_assistant(text);
    }

    fn write_assistant(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.mode == OutputMode::Json {
            self.output.push_str(text);
        } else {
            let _ = crate::write_stdout(text);
        }
        self.has_output = true;
        let trailing = text.bytes().rev().take_while(|byte| *byte == b'\n').count();
        self.trailing_newlines = if trailing == text.len() {
            (self.trailing_newlines + trailing.min(2)).min(2)
        } else {
            trailing.min(2)
        };
    }

    fn describe_failure(&mut self, failure: &TurnFailure) -> FailureSummary {
        let summary = match failure {
            TurnFailure::Provider(error) if error.status.is_some() => {
                self.describe_http_failure(error)
            }
            TurnFailure::Provider(error) => {
                self.describe_error(&error.code, error.detail.as_deref())
            }
            TurnFailure::InvalidCompletion => self.describe_error(failure.code(), None),
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
            format!("{CONFIGURED_SOURCE_LABEL} authentication failed · HTTP 401")
        } else {
            detail.to_owned()
        };
        write_stderr(&format!("oh-fx ask: {message}\n"))?;
        if auth_failure && detail != bare {
            write_stderr(&format!("oh-fx ask: {detail}\n"))?;
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
            model,
            resolved_provider: None,
            session_id: "",
            steps: self.steps,
            tool_calls: &self.tool_calls,
            usage: usage_record(report.usage),
            error: summary.error.as_deref(),
            auth_failure: summary.auth_failure.then_some(AuthFailureRecord {
                source: CONFIGURED_SOURCE_LABEL,
                reason: "http_unauthorized",
                http_status: 401,
            }),
            recovery: self.recovery.as_ref().map(RecoveryRecord::new),
        })
    }
}

#[cfg(test)]
mod tests {
    use ofx_contract::{
        CallDescription, Concurrency, ModelFailureDiagnostic, RouteRecoveryKind, ToolActivity,
        ToolCallId, ToolEffect, TurnId,
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
            ToolRecord::rejected("missing".to_owned()),
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
            r#"{"output":"","final_output":"","exit_code":1,"model":"","resolved_provider":null,"session_id":"","steps":0,"tool_calls":[{"name":"read_file","status":"success"},{"name":"read_file","status":"error","error":{"category":"tool_failed","code":"tool_failed"}},{"name":"missing","status":"error","error":{"category":"rejected","code":"rejected"}}],"usage":{"input_tokens":null,"output_tokens":null},"recovery":{"state":"recovered","kind":"auto_recovered","attempt":2,"attempt_limit":10,"delay_seconds":0,"durable":false,"message":"✓ recovered · succeeded on attempt 2"}}"#
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

    #[test]
    fn raw_output_separates_text_around_tool_steps_and_counts_rejections() {
        let mut presenter = Presenter::new(true);
        presenter.push_assistant("Looking.");
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
        presenter.push_assistant("Found it.");
        assert!(presenter.handle(UiEvent::ToolRejected {
            turn_id: TurnId::new(1),
            call_id: ToolCallId::new("call-2"),
            tool_name: "missing".to_owned(),
        }));
        presenter.push_assistant("\nDone");
        assert_eq!(presenter.output, "Looking.\n\nFound it.\n\n\nDone");
        assert_eq!(presenter.steps, 2);
        assert_eq!(presenter.tool_calls.len(), 1);
    }
}
