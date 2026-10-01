use crate::ids::ToolCallId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolResultStatus {
    Success,
    Failure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FinishReason {
    Stop,
    ToolCalls,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelRecoveryCause {
    NetworkInterrupted,
    ConnectivityLost,
    ProviderUnavailable,
    RateLimited,
}

impl ModelRecoveryCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NetworkInterrupted => "network_interrupted",
            Self::ConnectivityLost => "connectivity_lost",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::RateLimited => "rate_limited",
        }
    }

    const fn display(self) -> &'static str {
        match self {
            Self::NetworkInterrupted => "Network interrupted",
            Self::ConnectivityLost => "Connection lost",
            Self::ProviderUnavailable => "Provider unavailable",
            Self::RateLimited => "Rate limited",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelRecoveryAction {
    RetryingRequest,
    WaitingForConnectivity,
}

impl ModelRecoveryAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RetryingRequest => "retrying_request",
            Self::WaitingForConnectivity => "waiting_for_connectivity",
        }
    }

    const fn display(self) -> &'static str {
        match self {
            Self::RetryingRequest => "retrying request",
            Self::WaitingForConnectivity => "waiting for connection",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFailureDiagnostic(String);

impl ModelFailureDiagnostic {
    const MAX_BYTES: usize = 256;
    const MARKER: &'static str = "...";

    pub fn new(text: &str) -> Self {
        if text.len() <= Self::MAX_BYTES {
            return Self(text.to_owned());
        }
        let prefix = text.floor_char_boundary(Self::MAX_BYTES - Self::MARKER.len());
        Self(format!("{}{}", &text[..prefix], Self::MARKER))
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
}

impl RouteRecoveryKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AutoRetry => "auto_retry",
            Self::AutoRecovered => "auto_recovered",
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
    pub delay_seconds: u64,
    pub diagnostic: Option<ModelFailureDiagnostic>,
}

impl RouteRecoveryStatus {
    pub fn is_recovered(&self) -> bool {
        self.kind == RouteRecoveryKind::AutoRecovered
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
        if action == ModelRecoveryAction::WaitingForConnectivity {
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
            delay_seconds,
            diagnostic: diagnostic.map(ModelFailureDiagnostic::new),
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
    fn permission_mode_parse_accepts_upstream_spellings() {
        assert_eq!(PermissionMode::parse("ASK"), Some(PermissionMode::Ask));
        assert_eq!(PermissionMode::parse("auto"), Some(PermissionMode::Auto));
        for spelling in ["full-access", "Full Access", "YOLO"] {
            assert_eq!(PermissionMode::parse(spelling), Some(PermissionMode::Yolo));
        }
        assert_eq!(PermissionMode::parse("full_access"), None);
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
            delay_seconds: 0,
            diagnostic: None,
        };
        assert_eq!(recovered.label(), "✓ recovered · succeeded on attempt 2");
        assert_eq!(recovered.reported_attempt(), 2);
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
    }
}
