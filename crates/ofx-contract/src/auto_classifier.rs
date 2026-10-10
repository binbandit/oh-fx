use tokio_util::sync::CancellationToken;

use crate::stream_provider::{BoxFuture, Completion, ModelRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewFailure {
    ReviewerUnconfigured,
    InvalidContext,
    ConstructionTimedOut,
    ConstructionFailed,
    TransportTransient,
    TransportPermanent,
    TransportTimedOut,
    TurnReviewBudgetExhausted,
    CompletionText,
    CompletionToolCallCount,
    CompletionToolName,
    CompletionArgumentIntegrity,
    ArgumentsJson,
    ArgumentsShape,
    ArgumentsDecision,
}

impl ReviewFailure {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReviewerUnconfigured => "reviewer_unconfigured",
            Self::InvalidContext => "invalid_context",
            Self::ConstructionTimedOut => "construction_timed_out",
            Self::ConstructionFailed => "construction_failed",
            Self::TransportTransient => "transport_transient",
            Self::TransportPermanent => "transport_permanent",
            Self::TransportTimedOut => "transport_timed_out",
            Self::TurnReviewBudgetExhausted => "turn_review_budget_exhausted",
            Self::CompletionText => "completion_text",
            Self::CompletionToolCallCount => "completion_tool_call_count",
            Self::CompletionToolName => "completion_tool_name",
            Self::CompletionArgumentIntegrity => "completion_argument_integrity",
            Self::ArgumentsJson => "arguments_json",
            Self::ArgumentsShape => "arguments_shape",
            Self::ArgumentsDecision => "arguments_decision",
        }
    }

    pub const fn is_malformed_completion(self) -> bool {
        matches!(
            self,
            Self::CompletionText
                | Self::CompletionToolCallCount
                | Self::CompletionToolName
                | Self::CompletionArgumentIntegrity
                | Self::ArgumentsJson
                | Self::ArgumentsShape
                | Self::ArgumentsDecision
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReviewTransportOutcome {
    Completion(Box<Completion>),
    TransientFailure,
    PermanentFailure,
    TimedOut,
    Cancelled,
}

pub trait ReviewTransport: Send + Sync {
    fn model<'a>(&'a self, source_model: &'a str) -> &'a str;

    fn max_output_tokens(&self, model: &str) -> u32;

    fn request_body(&self, request: &ModelRequest<'_>) -> Option<String>;

    fn send<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        body: String,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ReviewTransportOutcome>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_failures_carry_upstream_cause_names() {
        assert_eq!(
            ReviewFailure::ReviewerUnconfigured.as_str(),
            "reviewer_unconfigured"
        );
        assert_eq!(
            ReviewFailure::TurnReviewBudgetExhausted.as_str(),
            "turn_review_budget_exhausted"
        );
        assert!(ReviewFailure::ArgumentsDecision.is_malformed_completion());
        assert!(!ReviewFailure::TransportTimedOut.is_malformed_completion());
    }
}
