use std::fmt;

use crate::types::{ChatMessage, ProviderReplay, ToolCall, ToolResultStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepResult<'a> {
    pub call_id: &'a str,
    pub tool_name: &'a str,
    pub output: &'a str,
    pub output_bytes: usize,
    pub status: ToolResultStatus,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryTurn<'a> {
    pub user: &'a str,
    pub steps: Vec<HistoryStep<'a>>,
    pub end: TurnEnd<'a>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryCut {
    pub turns: usize,
    pub tool_steps: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoredHistory {
    pub checkpoint: Option<String>,
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

pub trait ConversationLog: Send + Sync {
    fn require_writable(&self) -> Result<(), LogFailure>;

    fn record_turn(&mut self, turn: &HistoryTurn<'_>) -> Result<(), LogFailure>;

    fn record_compaction(
        &mut self,
        checkpoint: &str,
        cut: HistoryCut,
        active: Option<&HistoryTurn<'_>>,
    ) -> Result<(), LogFailure>;
}
