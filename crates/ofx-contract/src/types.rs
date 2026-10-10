mod question_batch;
mod tool_argument_integrity;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use ofx_text::{is_terminal_safe, mask_secrets};

use crate::ids::ToolCallId;

pub use question_batch::{QuestionBatchEntry, QuestionOption};
pub(crate) use tool_argument_integrity::ToolArgumentFailure;
pub use tool_argument_integrity::{ArgumentShape, ToolArgumentDiagnostic, ToolArgumentIntegrity};

const MAX_REASONING_EFFORT_NAME_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolExecutionProvenance {
    #[default]
    FxLocal,
    ProviderExecuted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: String,
    pub provider_result: Option<String>,
    pub provenance: ToolExecutionProvenance,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: ToolCallId::new(id),
            name: name.into(),
            arguments: arguments.into(),
            provider_result: None,
            provenance: ToolExecutionProvenance::FxLocal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolResultStatus {
    Success,
    Failure,
}

impl ToolResultStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnTokenProgress {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub input_exact: bool,
    pub output_exact: bool,
}

impl Default for TurnTokenProgress {
    fn default() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            input_exact: true,
            output_exact: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnSummary {
    pub started_at_ms: i64,
    pub completed_at_ms: i64,
    pub thinking_duration_ms: u64,
    pub turn_duration_ms: u64,
    pub token_progress: TurnTokenProgress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandProcessPresentation {
    ExitCode(i64),
    Signal(u32),
    TimedOut,
    OutputCaptureFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileChangeStats {
    pub additions: u32,
    pub deletions: u32,
}

impl FileChangeStats {
    pub fn from_lines(additions: usize, deletions: usize) -> Self {
        let count = |lines: usize| u32::try_from(lines).unwrap_or(u32::MAX);
        Self {
            additions: count(additions),
            deletions: count(deletions),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolStatusDetail {
    PreflightFailed,
    StalePreview,
    Cancelled,
    Rejected,
}

impl ToolStatusDetail {
    pub fn text(self) -> &'static str {
        match self {
            Self::PreflightFailed => "preflight failed",
            Self::StalePreview => "stale preview",
            Self::Cancelled => "cancelled",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaySource {
    pub provider: String,
    pub model: String,
    pub binding: Option<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderReplay {
    pub source: ReplaySource,
    pub parts_json: String,
}

impl ProviderReplay {
    pub fn matches(&self, source: &ReplaySource) -> bool {
        self.source.provider == source.provider && self.source.model == source.model
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageAttachment {
    pub id: u64,
    pub path: String,
    pub media_type: String,
    pub snapshot_path: Option<String>,
    pub snapshot_sha256: Option<String>,
    pub inline_data: Option<Vec<u8>>,
    pub source_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
        restored_steering: bool,
        feedback_for: Option<ToolCallId>,
        images: Vec<ImageAttachment>,
    },
    Assistant {
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
        provider_replay: Option<ProviderReplay>,
    },
    Tool {
        call_id: ToolCallId,
        tool_name: String,
        content: String,
        status: ToolResultStatus,
    },
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
            restored_steering: false,
            feedback_for: None,
            images: Vec::new(),
        }
    }

    pub fn restored_steering(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
            restored_steering: true,
            feedback_for: None,
            images: Vec::new(),
        }
    }

    pub fn permission_feedback(call_id: ToolCallId, content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
            restored_steering: false,
            feedback_for: Some(call_id),
            images: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
}

impl ToolChoice {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Required => "required",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl Usage {
    pub fn accumulate(&mut self, step: Usage) {
        self.input_tokens = add_known(self.input_tokens, step.input_tokens);
        self.output_tokens = add_known(self.output_tokens, step.output_tokens);
    }
}

fn add_known(total: Option<u64>, step: Option<u64>) -> Option<u64> {
    match (total, step) {
        (Some(total), Some(step)) => Some(total.saturating_add(step)),
        (total, step) => total.or(step),
    }
}

pub const CODEX_ORIGINATOR: &str = "oh-fx";
const MAX_CREDENTIAL_ACCOUNT_ID_BYTES: usize = 1024;

pub fn valid_credential_account_id(account_id: &str) -> bool {
    !account_id.is_empty()
        && account_id.len() <= MAX_CREDENTIAL_ACCOUNT_ID_BYTES
        && account_id.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

pub fn valid_gateway_generation_id(id: &str) -> bool {
    id.len() == GATEWAY_GENERATION_ID_BYTES
        && id
            .strip_prefix(GATEWAY_GENERATION_ID_PREFIX)
            .is_some_and(|suffix| {
                suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || crockford_letter(byte))
            })
}

const GATEWAY_GENERATION_ID_PREFIX: &str = "gen_";
const GATEWAY_GENERATION_ID_BYTES: usize = 30;

fn crockford_letter(byte: u8) -> bool {
    byte.is_ascii_uppercase() && !matches!(byte, b'I' | b'L' | b'O' | b'U')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FinishReason {
    Stop,
    ToolCalls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPresentationOutcome {
    Completed,
    Interrupted,
    Failed,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionMode {
    Ask,
    Auto,
    Yolo,
}

impl PermissionMode {
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.eq_ignore_ascii_case("ask") {
            return Some(Self::Ask);
        }
        if raw.eq_ignore_ascii_case("auto") {
            return Some(Self::Auto);
        }
        ["full-access", "full access", "yolo"]
            .iter()
            .any(|spelling| raw.eq_ignore_ascii_case(spelling))
            .then_some(Self::Yolo)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::Yolo => "yolo",
        }
    }

    pub const fn display_label(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::Yolo => "full access",
        }
    }

    const fn code(self) -> u8 {
        match self {
            Self::Ask => 0,
            Self::Auto => 1,
            Self::Yolo => 2,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Ask,
            1 => Self::Auto,
            _ => Self::Yolo,
        }
    }
}

pub const FULL_ACCESS_WARNING: &str = "Full access enabled: oh-fx permission checks disabled";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionAction {
    Allow,
    Ask,
    Deny,
}

impl PermissionAction {
    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Allow, Self::Ask, Self::Deny]
            .into_iter()
            .find(|action| raw.eq_ignore_ascii_case(action.label()))
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PermissionRule {
    pub permission: String,
    pub pattern: String,
    pub action: PermissionAction,
}

#[derive(Debug, Clone)]
pub struct LivePermissionMode(Arc<AtomicU8>);

impl LivePermissionMode {
    pub fn get(&self) -> PermissionMode {
        PermissionMode::from_code(self.0.load(Ordering::Acquire))
    }

    pub fn set(&self, mode: PermissionMode) {
        self.0.store(mode.code(), Ordering::Release);
    }
}

impl From<PermissionMode> for LivePermissionMode {
    fn from(mode: PermissionMode) -> Self {
        Self(Arc::new(AtomicU8::new(mode.code())))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningEffort {
    Auto,
    Named(String),
}

impl ReasoningEffort {
    pub fn parse(raw: &str) -> Option<Self> {
        if ["auto", "adaptive", "default"]
            .iter()
            .any(|alias| raw.eq_ignore_ascii_case(alias))
        {
            return Some(Self::Auto);
        }
        is_valid_reasoning_effort(raw).then(|| Self::Named(raw.to_owned()))
    }

    pub fn into_named(self) -> Option<String> {
        match self {
            Self::Auto => None,
            Self::Named(name) => Some(name),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Auto => "auto",
            Self::Named(name) => name,
        }
    }

    pub fn display_label(&self) -> &str {
        match self {
            Self::Auto => "default",
            Self::Named(name) => name,
        }
    }
}

pub fn is_valid_reasoning_effort(raw: &str) -> bool {
    (1..=MAX_REASONING_EFFORT_NAME_BYTES).contains(&raw.len())
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelRecoveryCause {
    NetworkInterrupted,
    ConnectivityLost,
    ProviderStreamTimeout,
    ProviderUnavailable,
    RateLimited,
}

impl ModelRecoveryCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NetworkInterrupted => "network_interrupted",
            Self::ConnectivityLost => "connectivity_lost",
            Self::ProviderStreamTimeout => "provider_stream_timeout",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::RateLimited => "rate_limited",
        }
    }

    const fn display(self) -> &'static str {
        match self {
            Self::NetworkInterrupted => "Network interrupted",
            Self::ConnectivityLost => "Connection lost",
            Self::ProviderStreamTimeout => "Gateway stream timed out",
            Self::ProviderUnavailable => "Provider unavailable",
            Self::RateLimited => "Rate limited",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelRecoveryAction {
    RetryingRequest,
    ContinuingResponse,
    RegeneratingTool,
    ContinuingAfterTool,
    ReconcilingTool,
    WaitingForConnectivity,
    CheckingLiveness,
    Paused,
}

impl ModelRecoveryAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RetryingRequest => "retrying_request",
            Self::ContinuingResponse => "continuing_response",
            Self::RegeneratingTool => "regenerating_tool",
            Self::ContinuingAfterTool => "continuing_after_tool",
            Self::ReconcilingTool => "reconciling_tool",
            Self::WaitingForConnectivity => "waiting_for_connectivity",
            Self::CheckingLiveness => "checking_liveness",
            Self::Paused => "paused",
        }
    }

    const fn display(self) -> &'static str {
        match self {
            Self::RetryingRequest => "retrying request",
            Self::ContinuingResponse => "restarting response",
            Self::RegeneratingTool => "regenerating unstarted tool",
            Self::ContinuingAfterTool => "continuing after confirmed tool",
            Self::ReconcilingTool => "checking uncertain tool state",
            Self::WaitingForConnectivity => "waiting for connection",
            Self::CheckingLiveness => "checking the connection",
            Self::Paused => "recovery paused",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ModelRecoveryRequiredAction {
    #[default]
    None,
    ContinueLater,
    InspectUncertainTool,
    SurfaceStall,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFailureDiagnostic(String);

impl ModelFailureDiagnostic {
    const MAX_BYTES: usize = 256;
    const MARKER: &'static str = "...";

    pub fn new(text: &str) -> Self {
        let text = mask_secrets(text);
        if text.len() <= Self::MAX_BYTES {
            return Self(text.into_owned());
        }
        let prefix = text.floor_char_boundary(Self::MAX_BYTES - Self::MARKER.len());
        Self(format!("{}{}", &text[..prefix], Self::MARKER))
    }

    pub fn restored(text: &str) -> Option<Self> {
        ((1..=Self::MAX_BYTES).contains(&text.len()) && is_terminal_safe(text.as_bytes()))
            .then(|| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn human_text(&self) -> &str {
        const PAIRS: [(&str, &str); 15] = [
            ("ReadFailed", "connection dropped"),
            ("WriteFailed", "connection dropped"),
            ("HttpConnectionClosing", "connection closed"),
            ("ConnectionResetByPeer", "connection reset"),
            ("ConnectionRefused", "connection refused"),
            ("ConnectionTimedOut", "connection timed out"),
            ("Timeout", "timed out"),
            ("StreamStalled", "stream stalled"),
            ("UnknownHostName", "cannot resolve host"),
            ("NameServerFailure", "DNS lookup failed"),
            ("NetworkUnreachable", "network unreachable"),
            ("NetworkDown", "network is down"),
            ("HostUnreachable", "host unreachable"),
            ("NetworkInterrupted", "network interrupted"),
            ("StreamInterrupted", "stream interrupted"),
        ];
        PAIRS
            .iter()
            .find(|(raw, _)| *raw == self.0)
            .map_or(&self.0, |(_, human)| human)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteRecoveryKind {
    AutoRetry,
    AutoRecovered,
    TerminalProviderError,
}

impl RouteRecoveryKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AutoRetry => "auto_retry",
            Self::AutoRecovered => "auto_recovered",
            Self::TerminalProviderError => "terminal_provider_error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRecoveryStatus {
    pub kind: RouteRecoveryKind,
    pub failed_attempt: usize,
    pub succeeded_attempt: usize,
    pub attempt_limit: usize,
    pub cause: Option<ModelRecoveryCause>,
    pub action: Option<ModelRecoveryAction>,
    pub required_action: ModelRecoveryRequiredAction,
    pub delay_seconds: u64,
    pub diagnostic: Option<ModelFailureDiagnostic>,
    pub retry_wait: Option<Duration>,
}

impl RouteRecoveryStatus {
    pub fn is_recovered(&self) -> bool {
        self.kind == RouteRecoveryKind::AutoRecovered
    }

    pub fn is_terminal(&self) -> bool {
        self.kind == RouteRecoveryKind::TerminalProviderError
    }

    pub fn is_paused(&self) -> bool {
        self.is_terminal() && self.action == Some(ModelRecoveryAction::Paused)
    }

    pub fn reported_attempt(&self) -> usize {
        if self.is_recovered() && self.succeeded_attempt != 0 {
            self.succeeded_attempt
        } else {
            self.failed_attempt
        }
    }

    pub fn label(&self) -> String {
        match self.kind {
            RouteRecoveryKind::AutoRetry => self.recovery_label(),
            RouteRecoveryKind::AutoRecovered => format!(
                "✓ recovered · succeeded on attempt {}",
                self.succeeded_attempt
            ),
            RouteRecoveryKind::TerminalProviderError => self.stopped_label(),
        }
    }

    fn stopped_label(&self) -> String {
        if self.required_action == ModelRecoveryRequiredAction::SurfaceStall {
            return self.stalled_label();
        }
        let attempt = self.failed_attempt;
        let Some(cause) = self.cause else {
            return self.stopped_cause_label(ModelRecoveryCause::ProviderUnavailable.display());
        };
        if cause == ModelRecoveryCause::RateLimited {
            return match &self.diagnostic {
                Some(diagnostic) => format!(
                    "⚠ Rate limited · {} · server requested a longer wait · recovery paused · attempt {attempt}",
                    diagnostic.as_str()
                ),
                None => format!(
                    "⚠ Rate limited · server requested a longer wait · recovery paused · attempt {attempt}"
                ),
            };
        }
        self.stopped_cause_label(cause.display())
    }

    fn stalled_label(&self) -> String {
        let cause = self
            .cause
            .map_or("Response failed", ModelRecoveryCause::display);
        match &self.diagnostic {
            Some(diagnostic) => format!(
                "⚠ {cause} · {} · kept failing at the same point · stopped",
                diagnostic.human_text()
            ),
            None => format!("⚠ {cause} · kept failing at the same point · stopped"),
        }
    }

    fn stopped_cause_label(&self, name: &str) -> String {
        let attempt = self.failed_attempt;
        let plural = if attempt == 1 { "" } else { "s" };
        let state = if self.action == Some(ModelRecoveryAction::Paused) {
            "recovery paused"
        } else {
            "stopped"
        };
        match &self.diagnostic {
            Some(diagnostic) => format!(
                "⚠ {name} · {} · {state} after {attempt} attempt{plural}",
                diagnostic.human_text()
            ),
            None => format!("⚠ {name} · {state} after {attempt} attempt{plural}"),
        }
    }

    fn recovery_label(&self) -> String {
        let cause = self
            .cause
            .unwrap_or(ModelRecoveryCause::ProviderUnavailable)
            .display();
        let action = self.action.unwrap_or(ModelRecoveryAction::RetryingRequest);
        let action_text = action.display();
        let delay = self.delay_seconds;
        if matches!(
            action,
            ModelRecoveryAction::WaitingForConnectivity | ModelRecoveryAction::CheckingLiveness
        ) {
            return if delay > 0 {
                format!("⚠ {cause} · {action_text} · {delay}s")
            } else {
                format!("⚠ {cause} · {action_text}")
            };
        }
        let diagnostic = self
            .diagnostic
            .as_ref()
            .map(ModelFailureDiagnostic::human_text);
        match (diagnostic, delay) {
            (Some(diagnostic), 0) => format!("⚠ {cause} · {diagnostic} · {action_text}"),
            (Some(diagnostic), delay) => {
                format!("⚠ {cause} · {diagnostic} · {action_text} in {delay}s")
            }
            (None, 0) => format!("⚠ {cause} · {action_text}"),
            (None, delay) => format!("⚠ {cause} · {action_text} in {delay}s"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_actions_parse_in_any_letter_case_and_keep_upstream_labels() {
        assert_eq!(
            PermissionAction::parse("ALLOW"),
            Some(PermissionAction::Allow)
        );
        assert_eq!(PermissionAction::parse("Ask"), Some(PermissionAction::Ask));
        assert_eq!(
            PermissionAction::parse("deny"),
            Some(PermissionAction::Deny)
        );
        assert_eq!(PermissionAction::parse("allowed"), None);
        assert_eq!(PermissionAction::parse(""), None);
        assert_eq!(PermissionAction::Deny.label(), "deny");
    }

    #[test]
    fn provider_replay_matches_only_its_own_provider_and_model() {
        let source = |provider: &str, model: &str| ReplaySource {
            provider: provider.to_owned(),
            model: model.to_owned(),
            binding: None,
        };
        let replay = ProviderReplay {
            source: source("codex", "model"),
            parts_json: "[]".to_owned(),
        };
        assert!(replay.matches(&source("codex", "model")));
        for other in [
            source("grok", "model"),
            source("gateway", "model"),
            source("codex", "different"),
        ] {
            assert!(!replay.matches(&other), "{other:?}");
        }
    }

    fn retry(cause: ModelRecoveryCause, delay_seconds: u64, diagnostic: Option<&str>) -> String {
        RouteRecoveryStatus {
            kind: RouteRecoveryKind::AutoRetry,
            failed_attempt: 1,
            succeeded_attempt: 0,
            attempt_limit: 10,
            cause: Some(cause),
            action: Some(if cause == ModelRecoveryCause::ConnectivityLost {
                ModelRecoveryAction::WaitingForConnectivity
            } else {
                ModelRecoveryAction::RetryingRequest
            }),
            required_action: ModelRecoveryRequiredAction::None,
            delay_seconds,
            diagnostic: diagnostic.map(ModelFailureDiagnostic::new),
            retry_wait: None,
        }
        .label()
    }

    #[test]
    fn usage_accumulates_only_reported_counters() {
        let mut usage = Usage::default();
        usage.accumulate(Usage {
            input_tokens: Some(10),
            output_tokens: None,
        });
        usage.accumulate(Usage {
            input_tokens: Some(5),
            output_tokens: Some(3),
        });
        assert_eq!(usage.input_tokens, Some(15));
        assert_eq!(usage.output_tokens, Some(3));
    }

    #[test]
    fn usage_skips_unreported_steps_and_stays_unknown_when_never_reported() {
        let mut usage = Usage::default();
        for input_tokens in [Some(10), None, Some(5)] {
            usage.accumulate(Usage {
                input_tokens,
                output_tokens: None,
            });
        }
        assert_eq!(usage.input_tokens, Some(15));
        assert_eq!(usage.output_tokens, None);
    }

    #[test]
    fn credential_account_ids_are_visible_ascii_header_values() {
        assert!(valid_credential_account_id("acct_123"));
        assert!(valid_credential_account_id(&"a".repeat(1024)));
        for invalid in ["", "acct 1", "acct\r\ninjected", "acct\u{7f}", "konto-é"] {
            assert!(!valid_credential_account_id(invalid), "{invalid:?}");
        }
        assert!(!valid_credential_account_id(&"a".repeat(1025)));
    }

    #[test]
    fn gateway_generation_ids_are_crockford_ulids_behind_the_gen_prefix() {
        assert!(valid_gateway_generation_id(
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV"
        ));
        assert!(valid_gateway_generation_id(
            "gen_0123456789ABCDEFGHJKMNPQRS"
        ));
        for invalid in [
            "",
            "gen_",
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FA",
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAVX",
            "GEN_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "gen-01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAv",
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAI",
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAL",
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAO",
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAU",
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FA_",
            "gen_01ARZ3NDEKTSV4RRFFQ69G5Fé",
        ] {
            assert!(!valid_gateway_generation_id(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn permission_mode_parse_accepts_upstream_spellings() {
        assert_eq!(PermissionMode::parse("ASK"), Some(PermissionMode::Ask));
        assert_eq!(PermissionMode::parse("auto"), Some(PermissionMode::Auto));
        for spelling in ["full-access", "Full Access", "YOLO"] {
            assert_eq!(PermissionMode::parse(spelling), Some(PermissionMode::Yolo));
        }
        assert_eq!(PermissionMode::parse("full_access"), None);
    }

    #[test]
    fn permission_modes_keep_upstream_labels_and_display_names() {
        let modes = [
            PermissionMode::Ask,
            PermissionMode::Auto,
            PermissionMode::Yolo,
        ];
        let labels: Vec<&str> = modes.iter().map(|mode| mode.label()).collect();
        assert_eq!(labels, ["ask", "auto", "yolo"]);
        let shown: Vec<&str> = modes.iter().map(|mode| mode.display_label()).collect();
        assert_eq!(shown, ["ask", "auto", "full access"]);
        for mode in modes {
            assert_eq!(PermissionMode::parse(mode.label()), Some(mode));
        }
    }

    #[test]
    fn a_live_permission_mode_is_shared_by_every_clone() {
        let live = LivePermissionMode::from(PermissionMode::Auto);
        let reader = live.clone();
        assert_eq!(reader.get(), PermissionMode::Auto);
        for mode in [
            PermissionMode::Yolo,
            PermissionMode::Ask,
            PermissionMode::Auto,
        ] {
            live.set(mode);
            assert_eq!(reader.get(), mode);
        }
    }

    #[test]
    fn reasoning_effort_parsing_folds_the_default_aliases_into_auto() {
        for alias in ["auto", "AUTO", "Adaptive", "default"] {
            assert_eq!(ReasoningEffort::parse(alias), Some(ReasoningEffort::Auto));
        }
        assert_eq!(
            ReasoningEffort::parse("xHigh"),
            Some(ReasoningEffort::Named("xHigh".to_owned()))
        );
        assert_eq!(ReasoningEffort::parse("very high"), None);
        assert_eq!(ReasoningEffort::Auto.into_named(), None);
        assert_eq!(
            ReasoningEffort::Named("low".to_owned()).into_named(),
            Some("low".to_owned())
        );
        assert_eq!(ReasoningEffort::parse("default").unwrap().label(), "auto");
        assert_eq!(ReasoningEffort::parse("xHigh").unwrap().label(), "xHigh");
        assert_eq!(ReasoningEffort::Auto.display_label(), "default");
        assert_eq!(
            ReasoningEffort::parse("xHigh").unwrap().display_label(),
            "xHigh"
        );
    }

    #[test]
    fn reasoning_effort_keeps_default_aliases_and_opaque_names() {
        for raw in [
            "auto",
            "AUTO",
            "adaptive",
            "Default",
            "none",
            "low",
            "xhigh",
            "future-tier",
            "v1.2_b",
        ] {
            assert!(is_valid_reasoning_effort(raw), "{raw:?}");
        }
        let longest = "e".repeat(MAX_REASONING_EFFORT_NAME_BYTES);
        assert!(is_valid_reasoning_effort(&longest));
        for invalid in ["", "contains space", "a/b", &format!("{longest}e")] {
            assert!(!is_valid_reasoning_effort(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn recovery_labels_match_upstream_wording() {
        assert_eq!(
            retry(
                ModelRecoveryCause::ProviderUnavailable,
                0,
                Some("HTTP 500 · boom")
            ),
            "⚠ Provider unavailable · HTTP 500 · boom · retrying request"
        );
        assert_eq!(
            retry(ModelRecoveryCause::RateLimited, 2, Some("HTTP 429 · slow")),
            "⚠ Rate limited · HTTP 429 · slow · retrying request in 2s"
        );
        assert_eq!(
            retry(
                ModelRecoveryCause::NetworkInterrupted,
                1,
                Some("ReadFailed")
            ),
            "⚠ Network interrupted · connection dropped · retrying request in 1s"
        );
        assert_eq!(
            retry(ModelRecoveryCause::NetworkInterrupted, 0, None),
            "⚠ Network interrupted · retrying request"
        );
        assert_eq!(
            retry(
                ModelRecoveryCause::ConnectivityLost,
                5,
                Some("ConnectionFailed")
            ),
            "⚠ Connection lost · waiting for connection · 5s"
        );
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
        assert_eq!(recovered.label(), "✓ recovered · succeeded on attempt 2");
        assert_eq!(recovered.reported_attempt(), 2);
    }

    fn stopped(
        cause: Option<ModelRecoveryCause>,
        failed_attempt: usize,
        diagnostic: Option<&str>,
    ) -> RouteRecoveryStatus {
        RouteRecoveryStatus {
            kind: RouteRecoveryKind::TerminalProviderError,
            failed_attempt,
            succeeded_attempt: 0,
            attempt_limit: 10,
            cause,
            action: None,
            required_action: ModelRecoveryRequiredAction::None,
            delay_seconds: 0,
            diagnostic: diagnostic.map(ModelFailureDiagnostic::new),
            retry_wait: None,
        }
    }

    #[test]
    fn terminal_labels_say_how_many_attempts_were_made() {
        let unavailable = Some(ModelRecoveryCause::ProviderUnavailable);
        assert_eq!(
            stopped(unavailable, 10, Some("HTTP 503 · overloaded")).label(),
            "⚠ Provider unavailable · HTTP 503 · overloaded · stopped after 10 attempts"
        );
        assert_eq!(
            stopped(unavailable, 1, Some("TestProviderSerializationFailed")).label(),
            "⚠ Provider unavailable · TestProviderSerializationFailed · stopped after 1 attempt"
        );
        assert_eq!(
            stopped(None, 3, None).label(),
            "⚠ Provider unavailable · stopped after 3 attempts"
        );
        assert_eq!(
            stopped(
                Some(ModelRecoveryCause::NetworkInterrupted),
                10,
                Some("ReadFailed")
            )
            .label(),
            "⚠ Network interrupted · connection dropped · stopped after 10 attempts"
        );
        assert_eq!(
            stopped(
                Some(ModelRecoveryCause::ConnectivityLost),
                10,
                Some("ConnectionRefused")
            )
            .label(),
            "⚠ Connection lost · connection refused · stopped after 10 attempts"
        );
        let limited = Some(ModelRecoveryCause::RateLimited);
        assert_eq!(
            stopped(limited, 10, Some("HTTP 429 · slow")).label(),
            "⚠ Rate limited · HTTP 429 · slow · server requested a longer wait · recovery paused · attempt 10"
        );
        assert_eq!(
            stopped(limited, 2, None).label(),
            "⚠ Rate limited · server requested a longer wait · recovery paused · attempt 2"
        );
        let terminal = stopped(unavailable, 10, None);
        assert!(terminal.is_terminal());
        assert_eq!(terminal.kind.as_str(), "terminal_provider_error");
        assert!(!stopped(unavailable, 10, None).is_recovered());
    }

    #[test]
    fn a_paused_recovery_says_how_many_attempts_it_made() {
        let mut paused = stopped(
            Some(ModelRecoveryCause::ConnectivityLost),
            1,
            Some("ConnectionFailed"),
        );
        paused.action = Some(ModelRecoveryAction::Paused);
        assert_eq!(
            paused.label(),
            "⚠ Connection lost · ConnectionFailed · recovery paused after 1 attempt"
        );
        paused.failed_attempt = 3;
        paused.diagnostic = None;
        assert_eq!(
            paused.label(),
            "⚠ Connection lost · recovery paused after 3 attempts"
        );
        assert_eq!(ModelRecoveryAction::Paused.as_str(), "paused");
        assert!(paused.is_paused());
        assert!(!stopped(None, 3, None).is_paused());
    }

    #[test]
    fn a_liveness_check_names_no_attempt_or_raw_error() {
        let probe = |delay_seconds| RouteRecoveryStatus {
            kind: RouteRecoveryKind::AutoRetry,
            action: Some(ModelRecoveryAction::CheckingLiveness),
            required_action: ModelRecoveryRequiredAction::None,
            delay_seconds,
            ..stopped(
                Some(ModelRecoveryCause::ProviderStreamTimeout),
                2,
                Some("Timeout"),
            )
        };
        assert_eq!(
            probe(0).label(),
            "⚠ Gateway stream timed out · checking the connection"
        );
        assert_eq!(
            probe(2).label(),
            "⚠ Gateway stream timed out · checking the connection · 2s"
        );
        assert_eq!(
            ModelRecoveryCause::ProviderStreamTimeout.as_str(),
            "provider_stream_timeout"
        );
        assert_eq!(
            ModelRecoveryAction::CheckingLiveness.as_str(),
            "checking_liveness"
        );
    }

    #[test]
    fn a_restarted_response_says_it_is_restarting() {
        let restarting = RouteRecoveryStatus {
            kind: RouteRecoveryKind::AutoRetry,
            action: Some(ModelRecoveryAction::ContinuingResponse),
            delay_seconds: 1,
            ..stopped(
                Some(ModelRecoveryCause::NetworkInterrupted),
                1,
                Some("ReadFailed"),
            )
        };
        assert_eq!(
            restarting.label(),
            "⚠ Network interrupted · connection dropped · restarting response in 1s"
        );
        assert_eq!(
            ModelRecoveryAction::ContinuingResponse.as_str(),
            "continuing_response"
        );
    }

    #[test]
    fn tool_recoveries_say_what_they_do_with_the_tool() {
        let retrying = |action| {
            RouteRecoveryStatus {
                kind: RouteRecoveryKind::AutoRetry,
                action: Some(action),
                ..stopped(
                    Some(ModelRecoveryCause::NetworkInterrupted),
                    1,
                    Some("ReadFailed"),
                )
            }
            .label()
        };
        assert_eq!(
            retrying(ModelRecoveryAction::RegeneratingTool),
            "⚠ Network interrupted · connection dropped · regenerating unstarted tool"
        );
        assert_eq!(
            retrying(ModelRecoveryAction::ContinuingAfterTool),
            "⚠ Network interrupted · connection dropped · continuing after confirmed tool"
        );
        assert_eq!(
            retrying(ModelRecoveryAction::ReconcilingTool),
            "⚠ Network interrupted · connection dropped · checking uncertain tool state"
        );
        let names = [
            ModelRecoveryAction::RegeneratingTool,
            ModelRecoveryAction::ContinuingAfterTool,
            ModelRecoveryAction::ReconcilingTool,
        ]
        .map(ModelRecoveryAction::as_str);
        assert_eq!(
            names,
            [
                "regenerating_tool",
                "continuing_after_tool",
                "reconciling_tool"
            ]
        );
    }

    #[test]
    fn a_stalled_recovery_says_it_kept_failing_at_the_same_point() {
        let mut stalled = stopped(
            Some(ModelRecoveryCause::NetworkInterrupted),
            3,
            Some("ReadFailed"),
        );
        stalled.required_action = ModelRecoveryRequiredAction::SurfaceStall;
        assert_eq!(
            stalled.label(),
            "⚠ Network interrupted · connection dropped · kept failing at the same point · stopped"
        );
        stalled.diagnostic = None;
        stalled.cause = Some(ModelRecoveryCause::ProviderStreamTimeout);
        assert_eq!(
            stalled.label(),
            "⚠ Gateway stream timed out · kept failing at the same point · stopped"
        );
        stalled.cause = None;
        assert_eq!(
            stalled.label(),
            "⚠ Response failed · kept failing at the same point · stopped"
        );
        assert!(stalled.is_terminal());
        assert!(!stalled.is_paused());
    }

    #[test]
    fn failure_diagnostics_are_bounded_at_a_character_boundary() {
        let long = "é".repeat(200);
        let diagnostic = ModelFailureDiagnostic::new(&long);
        assert!(diagnostic.0.len() <= 256);
        assert!(diagnostic.0.ends_with("..."));
        assert_eq!(
            ModelFailureDiagnostic::new("Timeout").human_text(),
            "timed out"
        );
        assert_eq!(ModelFailureDiagnostic::new("Timeout").as_str(), "Timeout");
    }

    #[test]
    fn failure_diagnostics_mask_a_credential_before_cutting_it() {
        let lead = "x".repeat(240);
        let text = format!("{lead} sk-proj-abcdefghijklmnopqrstuvwxyz0123456789 tail");
        assert_eq!(
            ModelFailureDiagnostic::new(&text).as_str(),
            format!("{lead} [redacted] tail")
        );
    }
}
