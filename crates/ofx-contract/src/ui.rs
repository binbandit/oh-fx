use crate::ids::{ToolCallId, TurnId};
use crate::tool_dispatch::CallDescription;
use crate::types::{RouteRecoveryStatus, ToolResultStatus, Usage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnOutcome {
    Completed,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRejection {
    Unsupported,
    Invalid,
    Panicked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    TurnStarted {
        turn_id: TurnId,
    },
    AssistantText {
        turn_id: TurnId,
        text: String,
    },
    ReasoningText {
        turn_id: TurnId,
        text: String,
    },
    Operational {
        turn_id: TurnId,
        text: String,
    },
    Recovery {
        turn_id: TurnId,
        status: RouteRecoveryStatus,
    },
    ToolStarted {
        turn_id: TurnId,
        call_id: ToolCallId,
        tool_name: String,
        description: CallDescription,
    },
    ToolFinished {
        turn_id: TurnId,
        call_id: ToolCallId,
        tool_name: String,
        arguments: String,
        status: ToolResultStatus,
        content: String,
        command_result: Option<String>,
    },
    ToolRejected {
        turn_id: TurnId,
        call_id: ToolCallId,
        tool_name: String,
        arguments: String,
        reason: ToolRejection,
    },
    UsageReported {
        turn_id: TurnId,
        usage: Usage,
    },
    TurnFinished {
        turn_id: TurnId,
        outcome: TurnOutcome,
    },
}
