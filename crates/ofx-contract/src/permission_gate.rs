use std::path::{Path, PathBuf};

use crate::applicable_target::ApplicableTarget;
use crate::types::ToolCall;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathAccess {
    WorkspaceOnly,
    WorkspaceOrExternal,
    Within(PathBuf),
}

impl PathAccess {
    pub fn confining_root<'a>(&'a self, workspace_root: &'a Path) -> Option<&'a Path> {
        match self {
            Self::WorkspaceOnly => Some(workspace_root),
            Self::WorkspaceOrExternal => None,
            Self::Within(root) => Some(root),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    SendInput {
        input: String,
    },
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatedAction<'a> {
    Call(&'a ToolCall),
    FileMutation(&'a FileMutation),
    Command(&'a CommandRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApprovalScope {
    pub target: Option<PathBuf>,
    pub access: PathAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApprovalDecision {
    Once,
    Always,
    Deny,
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

    fn approval_scope(&self, _action: GatedAction<'_>) -> ApprovalScope {
        ApprovalScope {
            target: None,
            access: PathAccess::WorkspaceOrExternal,
        }
    }

    fn remember_approval(&self, _action: GatedAction<'_>, _access: &PathAccess) {}
}
