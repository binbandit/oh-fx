use std::path::PathBuf;

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

pub trait PermissionGate: Send + Sync {
    fn admit(&self, call: &ToolCall) -> Admission;

    fn admit_file_mutation(&self, _mutation: &FileMutation) -> Admission {
        Admission::ApprovalRequired
    }

    fn admit_command(&self, _request: &CommandRequest) -> Admission {
        Admission::ApprovalRequired
    }
}
