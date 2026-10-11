use ofx_contract::{GatedAction, PermissionGate, PermissionMode, PermissionTraceFacts, ToolCall};
use ofx_trace::{TraceContext, trace_event};

const PERMISSION: &str = "permission";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Granted {
    Once,
    Always,
    Deny,
    PermissionRequired,
}

impl Granted {
    const fn name(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Always => "always",
            Self::Deny => "deny",
            Self::PermissionRequired => "permission_required",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Asked<'a> {
    pub(super) context: TraceContext,
    pub(super) call: &'a ToolCall,
    pub(super) action: GatedAction<'a>,
    pub(super) facts: PermissionTraceFacts,
}

const fn mode_name(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => "ask",
        PermissionMode::Auto => "auto",
        PermissionMode::Yolo => "yolo",
    }
}

pub(super) fn requested(
    context: TraceContext,
    call: &ToolCall,
    gate: &dyn PermissionGate,
    action: GatedAction<'_>,
) -> PermissionTraceFacts {
    if !ofx_trace::enabled(PERMISSION) {
        return PermissionTraceFacts::default();
    }
    let facts = gate.trace_facts(action);
    let (id, name, mode) = (call.id.as_str(), &call.name, mode_name(facts.mode));
    let outside = facts.outside_workspace;
    trace_event!(
        PERMISSION,
        "before_permission_wait",
        context,
        "call_id={id} tool_name={name} permission_mode={mode} local_grants={} outside_workspace={outside}",
        facts.local_grants
    );
    trace_event!(
        PERMISSION,
        "permission_requested",
        context,
        "call_id={id} tool_name={name} permission_mode={mode} outside_workspace={outside}"
    );
    facts
}

pub(super) fn decided(asked: &Asked<'_>, decision: Granted) {
    let (id, name) = (asked.call.id.as_str(), &asked.call.name);
    let mode = mode_name(asked.facts.mode);
    let (decision, source) = (decision.name(), source(asked, decision));
    let outside = asked.facts.outside_workspace;
    for event in ["after_permission_decision", "permission_decision"] {
        trace_event!(
            PERMISSION,
            event,
            asked.context,
            "call_id={id} tool_name={name} permission_mode={mode} decision={decision} approval_source={source} outside_workspace={outside}"
        );
    }
}

pub(super) fn cancelled(asked: &Asked<'_>) {
    trace_event!(
        PERMISSION,
        "permission_error",
        asked.context,
        "call_id={} tool_name={} permission_mode={} err=Cancelled outside_workspace={}",
        asked.call.id.as_str(),
        asked.call.name,
        mode_name(asked.facts.mode),
        asked.facts.outside_workspace
    );
}

fn source(asked: &Asked<'_>, decision: Granted) -> &'static str {
    match (decision, asked.action) {
        (Granted::Deny | Granted::PermissionRequired, _) => "denied",
        (_, GatedAction::FileMutation(_)) => "file_mutation",
        _ if asked.facts.local_grants > 0 => "session_grant_or_rule",
        _ => match asked.facts.mode {
            PermissionMode::Auto => "auto_or_rule",
            PermissionMode::Ask => "interactive_or_rule",
            PermissionMode::Yolo => "yolo",
        },
    }
}

#[cfg(test)]
mod tests;
