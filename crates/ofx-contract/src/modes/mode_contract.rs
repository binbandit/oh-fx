use crate::types::PermissionMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPolicy {
    Full,
    ReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub permission_mode: PermissionMode,
    pub tool_policy: ToolPolicy,
    pub tool_policy_denial_message: Option<&'static str>,
}
