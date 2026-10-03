use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::applicable_target::ApplicableTarget;
use crate::ids::{ToolCallId, TurnId};
use crate::permission_gate::{
    CommandRequest, FileChange, FileMutation, PathAccess, RootUserRequests,
};
use crate::stream_provider::BoxFuture;
use crate::subagent::SubagentStatusSink;
use crate::types::{
    CommandProcessPresentation, FileChangeStats, QuestionBatchEntry, ToolResultStatus,
    ToolStatusDetail,
};

#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolActivity {
    Read,
    List,
    Write,
    Edit,
    Open,
    Command,
    Subagent,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolEffect {
    None,
    ReadOnly,
    Mutating,
    Irreversible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Concurrency {
    Parallel,
    Serial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallPresentation {
    pub activity: ToolActivity,
    pub action_label: &'static str,
    pub completed_label: &'static str,
    pub label_argument: &'static str,
    pub label_default: &'static str,
}

impl CallPresentation {
    pub fn label(&self, target: impl Into<String>) -> ActionLabel {
        ActionLabel {
            active: self.action_label,
            completed: self.completed_label,
            target: target.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionLabel {
    pub active: &'static str,
    pub completed: &'static str,
    pub target: String,
}

impl ActionLabel {
    pub fn title(&self) -> String {
        format!("{} {}", self.active, self.target)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallDescription {
    pub title: String,
    pub label: Option<ActionLabel>,
    pub activity: ToolActivity,
    pub effect: ToolEffect,
    pub concurrency: Concurrency,
}

impl CallDescription {
    pub fn relabel(&mut self, label: ActionLabel) {
        self.title = label.title();
        self.label = Some(label);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub status: ToolResultStatus,
    pub content: String,
    pub command_result: Option<String>,
    pub context_notices: Vec<String>,
    pub process: Option<CommandProcessPresentation>,
    pub status_detail: Option<ToolStatusDetail>,
    pub file_change: Option<FileChangeStats>,
}

impl ToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            status: ToolResultStatus::Success,
            content: content.into(),
            command_result: None,
            context_notices: Vec::new(),
            process: None,
            status_detail: None,
            file_change: None,
        }
    }

    pub fn failure(content: impl Into<String>) -> Self {
        Self {
            status: ToolResultStatus::Failure,
            content: content.into(),
            command_result: None,
            context_notices: Vec::new(),
            process: None,
            status_detail: None,
            file_change: None,
        }
    }

    #[must_use]
    pub fn with_command_result(mut self, command_result: Option<String>) -> Self {
        self.command_result = command_result;
        self
    }

    #[must_use]
    pub fn with_context_notices(mut self, notices: impl IntoIterator<Item = String>) -> Self {
        self.context_notices.extend(notices);
        self
    }

    #[must_use]
    pub fn with_process(mut self, process: Option<CommandProcessPresentation>) -> Self {
        self.process = process;
        self
    }

    #[must_use]
    pub fn with_status_detail(mut self, detail: ToolStatusDetail) -> Self {
        self.status_detail = Some(detail);
        self
    }

    #[must_use]
    pub fn with_file_change(mut self, change: FileChangeStats) -> Self {
        self.file_change = Some(change);
        self
    }
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ToolContext {
    pub call_id: ToolCallId,
    pub cancellation: CancellationToken,
    pub path_access: PathAccess,
    pub root_user_requests: Option<Arc<RootUserRequests>>,
    pub turn_id: Option<TurnId>,
    pub subagent_status: Option<SubagentStatusSink>,
}

impl ToolContext {
    pub fn new(
        call_id: ToolCallId,
        cancellation: CancellationToken,
        path_access: PathAccess,
    ) -> Self {
        Self {
            call_id,
            cancellation,
            path_access,
            root_user_requests: None,
            turn_id: None,
            subagent_status: None,
        }
    }

    #[must_use]
    pub fn with_root_user_requests(mut self, requests: Arc<RootUserRequests>) -> Self {
        self.root_user_requests = Some(requests);
        self
    }

    #[must_use]
    pub fn with_turn(mut self, turn_id: TurnId) -> Self {
        self.turn_id = Some(turn_id);
        self
    }

    #[must_use]
    pub fn with_subagent_status(mut self, sink: SubagentStatusSink) -> Self {
        self.subagent_status = Some(sink);
        self
    }
}

pub trait QuestionAsker: Send + Sync {
    fn ask(&self, entries: Vec<QuestionBatchEntry>) -> BoxFuture<'static, Option<Vec<String>>>;
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;

    fn provider_executed(&self) -> bool {
        false
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput>;

    fn history_arguments(&self, _arguments: &str) -> Option<String> {
        None
    }
}

pub trait PreparedCall: Send {
    fn describe(&self) -> CallDescription;

    fn untargeted_label(&self) -> Option<ActionLabel> {
        None
    }

    fn complete(&mut self) {}

    fn applicable_target(&self) -> Option<ApplicableTarget> {
        None
    }

    fn file_mutation(&self) -> Option<&FileMutation> {
        None
    }

    fn file_change(&self) -> Option<FileChange<'_>> {
        None
    }

    fn command_request(&self) -> Option<&CommandRequest> {
        None
    }

    fn refusal(&self) -> Option<&ToolOutput> {
        None
    }

    fn execute(self: Box<Self>, context: ToolContext) -> BoxFuture<'static, ToolOutput>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outputs_carry_their_status() {
        assert_eq!(ToolOutput::success("ok").status, ToolResultStatus::Success);
        assert_eq!(
            ToolOutput::failure("Not executed").status,
            ToolResultStatus::Failure
        );
    }
}
