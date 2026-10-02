use tokio_util::sync::CancellationToken;

use crate::applicable_target::ApplicableTarget;
use crate::ids::ToolCallId;
use crate::permission_gate::{CommandRequest, FileChange, FileMutation, PathAccess};
use crate::stream_provider::BoxFuture;
use crate::types::ToolResultStatus;

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
    pub label_argument: &'static str,
    pub label_default: &'static str,
}

impl CallPresentation {
    pub fn untargeted_title(&self) -> String {
        format!("{} {}", self.action_label, self.label_default)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallDescription {
    pub title: String,
    pub activity: ToolActivity,
    pub effect: ToolEffect,
    pub concurrency: Concurrency,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub status: ToolResultStatus,
    pub content: String,
    pub command_result: Option<String>,
}

impl ToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            status: ToolResultStatus::Success,
            content: content.into(),
            command_result: None,
        }
    }

    pub fn failure(content: impl Into<String>) -> Self {
        Self {
            status: ToolResultStatus::Failure,
            content: content.into(),
            command_result: None,
        }
    }

    #[must_use]
    pub fn with_command_result(mut self, command_result: Option<String>) -> Self {
        self.command_result = command_result;
        self
    }
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ToolContext {
    pub call_id: ToolCallId,
    pub cancellation: CancellationToken,
    pub path_access: PathAccess,
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
        }
    }
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput>;

    fn history_arguments(&self, _arguments: &str) -> Option<String> {
        None
    }
}

pub trait PreparedCall: Send {
    fn describe(&self) -> CallDescription;

    fn untargeted_title(&self) -> String {
        self.describe().title
    }

    fn complete(&mut self) {}

    fn applicable_target(&self) -> Option<ApplicableTarget> {
        None
    }

    fn file_mutation(&self) -> Option<&FileMutation> {
        None
    }

    fn file_change(&self) -> Option<FileChange> {
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
