use crate::mcp_contract::{McpServerConfig, WorkspaceAdmission};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupPhase {
    All,
    AskStartup,
    AskDeferred,
    AcpStartup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupDecision {
    Connect,
    Deferred,
    Disabled,
}

pub fn decide_startup(config: &McpServerConfig, phase: StartupPhase) -> StartupDecision {
    decide(
        config.enabled,
        config.required,
        config.workspace_admission,
        phase,
    )
}

fn decide(
    enabled: bool,
    required: bool,
    workspace_admission: Option<WorkspaceAdmission>,
    phase: StartupPhase,
) -> StartupDecision {
    if !enabled {
        return StartupDecision::Disabled;
    }
    let deferred_when = |condition: bool| {
        if condition {
            StartupDecision::Deferred
        } else {
            StartupDecision::Connect
        }
    };
    match workspace_admission {
        Some(WorkspaceAdmission::Pending | WorkspaceAdmission::Rejected) => {
            StartupDecision::Disabled
        }
        Some(WorkspaceAdmission::Approved) => deferred_when(phase == StartupPhase::AskDeferred),
        None => match phase {
            StartupPhase::All | StartupPhase::AcpStartup => StartupDecision::Connect,
            StartupPhase::AskStartup => deferred_when(!required),
            StartupPhase::AskDeferred => deferred_when(required),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_admission_keeps_ask_required_servers_eager_and_optional_servers_deferred() {
        assert_eq!(
            decide(true, true, None, StartupPhase::All),
            StartupDecision::Connect
        );
        assert_eq!(
            decide(true, false, None, StartupPhase::All),
            StartupDecision::Connect
        );
        assert_eq!(
            decide(true, true, None, StartupPhase::AskStartup),
            StartupDecision::Connect
        );
        assert_eq!(
            decide(true, false, None, StartupPhase::AskStartup),
            StartupDecision::Deferred
        );
        assert_eq!(
            decide(true, true, None, StartupPhase::AskDeferred),
            StartupDecision::Deferred
        );
        assert_eq!(
            decide(true, false, None, StartupPhase::AskDeferred),
            StartupDecision::Connect
        );
        assert_eq!(
            decide(true, true, None, StartupPhase::AcpStartup),
            StartupDecision::Connect
        );
        assert_eq!(
            decide(true, false, None, StartupPhase::AcpStartup),
            StartupDecision::Connect
        );
    }

    #[test]
    fn disabled_servers_never_enter_a_connection_phase() {
        assert_eq!(
            decide(false, true, None, StartupPhase::All),
            StartupDecision::Disabled
        );
        assert_eq!(
            decide(
                false,
                true,
                Some(WorkspaceAdmission::Pending),
                StartupPhase::AskStartup
            ),
            StartupDecision::Disabled
        );
        assert_eq!(
            decide(
                false,
                false,
                Some(WorkspaceAdmission::Approved),
                StartupPhase::AskDeferred
            ),
            StartupDecision::Disabled
        );
        assert_eq!(
            decide(
                false,
                false,
                Some(WorkspaceAdmission::Rejected),
                StartupPhase::AcpStartup
            ),
            StartupDecision::Disabled
        );
    }

    #[test]
    fn workspace_admission_is_phase_derived_and_reject_is_absorbing() {
        use StartupDecision::{Connect, Deferred, Disabled};
        let cases = [
            (
                WorkspaceAdmission::Approved,
                [Connect, Connect, Deferred, Connect],
            ),
            (
                WorkspaceAdmission::Pending,
                [Disabled, Disabled, Disabled, Disabled],
            ),
            (
                WorkspaceAdmission::Rejected,
                [Disabled, Disabled, Disabled, Disabled],
            ),
        ];
        let phases = [
            StartupPhase::All,
            StartupPhase::AskStartup,
            StartupPhase::AskDeferred,
            StartupPhase::AcpStartup,
        ];
        for (admission, expected) in cases {
            for (phase, decision) in phases.into_iter().zip(expected) {
                assert_eq!(decide(true, false, Some(admission), phase), decision);
                assert_eq!(decide(true, true, Some(admission), phase), decision);
            }
        }
    }
}
