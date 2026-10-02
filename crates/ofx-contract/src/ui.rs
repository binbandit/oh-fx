use crate::ids::{RequestId, ToolCallId, TurnId};
use crate::permission_gate::{ApprovalDecision, ApprovalScope, CommandRequest, FileMutation};
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
    MalformedArguments,
    Invalid,
    Panicked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoticeTone {
    Information,
    Success,
    Warning,
    Error,
    Cancelled,
    Neutral,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeLink {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub topic: String,
    pub tone: NoticeTone,
    pub body: String,
    pub link: Option<NoticeLink>,
}

impl Notice {
    pub fn new(tone: NoticeTone, topic: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            topic: topic.into(),
            tone,
            body: body.into(),
            link: None,
        }
    }

    #[must_use]
    pub fn with_link(mut self, label: impl Into<String>, url: impl Into<String>) -> Self {
        self.link = Some(NoticeLink {
            label: label.into(),
            url: url.into(),
        });
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub id: RequestId,
    pub tool_name: String,
    pub title: String,
    pub tool_arguments_preview: String,
    pub scope: ApprovalScope,
    pub command: Option<CommandRequest>,
    pub file: Option<FileMutation>,
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
        title: Option<String>,
    },
    ContextNotice {
        turn_id: TurnId,
        text: String,
    },
    ApprovalRequested {
        turn_id: TurnId,
        request: ApprovalRequest,
    },
    UsageReported {
        turn_id: TurnId,
        usage: Usage,
    },
    TurnFinished {
        turn_id: TurnId,
        outcome: TurnOutcome,
    },
    ApiStatus {
        turn_id: TurnId,
        text: String,
    },
    Notice {
        notice: Notice,
    },
    ModelSelected {
        model: String,
    },
    HelpRequested,
    ConversationCleared {
        first_kept_prompt: u64,
    },
    ExitRequested,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    Submit {
        prompt: String,
    },
    RunCommand {
        text: String,
    },
    Cancel {
        turn_id: TurnId,
    },
    Approval {
        request_id: RequestId,
        decision: ApprovalDecision,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_carry_their_tone_topic_and_optional_link() {
        let plain = Notice::new(NoticeTone::Warning, "model", "unknown model");
        assert_eq!(plain.topic, "model");
        assert_eq!(plain.tone, NoticeTone::Warning);
        assert_eq!(plain.body, "unknown model");
        assert_eq!(plain.link, None);
        let linked =
            Notice::new(NoticeTone::Success, "", "updated").with_link("notes", "https://x");
        assert_eq!(
            linked.link,
            Some(NoticeLink {
                label: "notes".to_owned(),
                url: "https://x".to_owned(),
            })
        );
    }
}
