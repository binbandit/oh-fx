use std::path::PathBuf;

use crate::applicable_target::ApplicableTarget;
use crate::types::ToolCall;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathAccess {
    WorkspaceOnly,
    WorkspaceOrExternal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Admission {
    Allowed(PathAccess),
    ApprovalRequired,
    ReviewUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileMutationState {
    Creates,
    Changes,
    Unchanged,
    Unread,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileMutation {
    pub target: PathBuf,
    pub state: FileMutationState,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CommandRequest {
    Run {
        command: String,
        cwd: PathBuf,
        terminal: bool,
    },
    Observe,
    SendInput,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatedAction<'a> {
    Call(&'a ToolCall),
    FileMutation(&'a FileMutation),
    Command(&'a CommandRequest),
}

pub trait PermissionGate: Send + Sync {
    fn admit(&self, call: &ToolCall) -> Admission;

    fn applicable_target(&self, call: &ToolCall) -> Option<ApplicableTarget>;

    fn admit_file_mutation(&self, _mutation: &FileMutation) -> Admission {
        Admission::ApprovalRequired
    }

    fn admit_command(&self, _request: &CommandRequest) -> Admission {
        Admission::ApprovalRequired
    }

    fn remember_approval(&self, _action: GatedAction<'_>) {}
}
