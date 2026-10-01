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
}

pub trait PermissionGate: Send + Sync {
    fn admit(&self, call: &ToolCall) -> Admission;
}
