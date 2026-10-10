use std::fmt;
use std::time::Instant;

use crate::file_evidence::FileEvidence;
use crate::ids::{ToolCallId, TurnId};
use crate::types::{
    ChatMessage, CommandProcessPresentation, FileChangeStats, ModelRecoveryAction,
    ModelRecoveryCause, ProviderReplay, ToolCall, ToolResultStatus,
};

pub const INTERRUPTED_BEFORE_COMPLETION: &str = "The previous response ended before completion.";
pub const INTERRUPTED_TURN_CONTEXT: &str = "<turn_aborted>\nThe previous turn ended before completion. Any tools or commands may have partially executed. Do not continue this request unless the user explicitly asks to continue.\n</turn_aborted>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult<'a> {
    pub call_id: &'a str,
    pub tool_name: &'a str,
    pub output: &'a str,
    pub output_bytes: usize,
    pub status: ToolResultStatus,
    pub model_view_covers_full_file: bool,
    pub process: Option<CommandProcessPresentation>,
    pub permission_feedback: Vec<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryStep<'a> {
    pub assistant: &'a str,
    pub provider_replay: Option<&'a ProviderReplay>,
    pub tool_calls: &'a [ToolCall],
    pub tool_results: Vec<StepResult<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStop {
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEnd<'a> {
    Replied {
        text: &'a str,
        provider_replay: Option<&'a ProviderReplay>,
    },
    Stopped {
        reason: TurnStop,
        partial: &'a str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistorySteering<'a> {
    pub text: &'a str,
    pub assistant_prefix: &'a str,
    pub after_tool_step_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryTurn<'a> {
    pub user: &'a str,
    pub steps: Vec<HistoryStep<'a>>,
    pub steering: Vec<HistorySteering<'a>>,
    pub files: &'a [FileEvidence],
    pub end: TurnEnd<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStrategy {
    RetryRequest,
    ContinueResponse,
    RegenerateTool,
    ContinueAfterTool,
    ReconcileTool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedOutput {
    pub call_id: ToolCallId,
    pub bytes: usize,
    pub whole_file: bool,
    pub process: Option<CommandProcessPresentation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredTurn {
    pub prompt: String,
    pub messages: Vec<ChatMessage>,
    pub files: Vec<FileEvidence>,
    pub outputs: Vec<RecordedOutput>,
    pub source: String,
    pub source_presented: bool,
    pub cause: Option<ModelRecoveryCause>,
    pub tool_state: RecoveryToolState,
    pub strategy: RecoveryStrategy,
    pub fast_mode: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryToolState {
    None,
    ProvenUnexecuted,
    Confirmed,
    Uncertain,
}

impl RecoveryToolState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ProvenUnexecuted => "proven_unexecuted",
            Self::Confirmed => "confirmed",
            Self::Uncertain => "uncertain",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryProgress {
    Waiting(ModelRecoveryAction),
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryPoint<'a> {
    pub turn_id: TurnId,
    pub turn: HistoryTurn<'a>,
    pub source: &'a str,
    pub cause: ModelRecoveryCause,
    pub progress: RecoveryProgress,
    pub tool_state: RecoveryToolState,
    pub model: &'a str,
    pub requested_fast_mode: bool,
    pub fast_mode: bool,
    pub ultrafast_mode: bool,
    pub attempt_limit: usize,
    pub consumed_attempts: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryCut {
    pub turns: usize,
    pub tool_steps: usize,
    pub steering: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoredHistory {
    pub checkpoint: Option<String>,
    pub compaction_count: usize,
    pub messages: Vec<ChatMessage>,
    pub turn_starts: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFailure {
    pub code: String,
}

impl fmt::Display for LogFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.code)
    }
}

impl std::error::Error for LogFailure {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Unbilled,
    PossiblyBilledWithoutIdentity,
    AmbiguousDelivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestTicket {
    pub sequence: u64,
    pub started_at: Instant,
}

pub trait ConversationLog: Send + Sync {
    fn require_writable(&self) -> Result<(), LogFailure>;

    fn record_turn(&mut self, turn: &HistoryTurn<'_>) -> Result<(), LogFailure>;

    fn record_compaction(
        &mut self,
        checkpoint: &str,
        cut: HistoryCut,
        active: Option<&HistoryTurn<'_>>,
    ) -> Result<(), LogFailure>;

    fn record_recovery(&self, point: &RecoveryPoint<'_>) -> Result<(), LogFailure>;

    fn clear_recovery(&self) -> Result<(), LogFailure>;

    fn begin_request(&self) -> Result<RequestTicket, LogFailure>;

    fn finish_request(
        &self,
        ticket: RequestTicket,
        outcome: DeliveryOutcome,
    ) -> Result<(), LogFailure>;

    fn record_committed_lines(&self, change: FileChangeStats);
}
