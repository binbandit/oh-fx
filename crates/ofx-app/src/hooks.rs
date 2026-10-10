use std::sync::Arc;

use ofx_contract::{AttentionKind, HookRegistrationError, HookRuntime, HookScope, HookView};
use ofx_tui::{ForegroundLifecycle, ForegroundState};

pub(crate) fn configure<Client: ForegroundLifecycle + 'static>(
    herdr: Option<&Arc<Client>>,
) -> Result<HookView, HookRegistrationError> {
    let mut runtime = HookRuntime::default();
    if let Some(herdr) = herdr {
        register(&mut runtime, Arc::clone(herdr))?;
    }
    Ok(runtime.freeze())
}

fn register<Client: ForegroundLifecycle + 'static>(
    runtime: &mut HookRuntime,
    herdr: Arc<Client>,
) -> Result<(), HookRegistrationError> {
    runtime.register_attention_required("fx.herdr.attention_required", move |input| {
        if input.invocation.scope == HookScope::Interactive {
            herdr.report(ForegroundState::Blocked, Some(attention_status(input.kind)));
        }
    })
}

fn attention_status(kind: AttentionKind) -> &'static [u8] {
    match kind {
        AttentionKind::Permission => b"permission",
        AttentionKind::Question => b"question",
        AttentionKind::RouteRecovery => b"recovery",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ofx_contract::{AttentionKind, AttentionRequiredInput, HookInvocation, HookScope, TurnId};
    use ofx_tui::{ForegroundLifecycle, ForegroundState};

    use super::configure;

    type Report = (&'static str, Option<Vec<u8>>);

    #[derive(Default)]
    struct Recording(Mutex<Vec<Report>>);

    impl ForegroundLifecycle for Recording {
        fn report(&self, state: ForegroundState, status: Option<&[u8]>) {
            let state = match state {
                ForegroundState::Idle => "idle",
                ForegroundState::Working => "working",
                ForegroundState::Blocked => "blocked",
            };
            self.0
                .lock()
                .unwrap()
                .push((state, status.map(<[u8]>::to_vec)));
        }

        fn shutdown(&self) {}
    }

    #[test]
    fn herdr_attention_hooks_report_blocked_only_for_interactive_prompts() {
        let client = Arc::new(Recording::default());
        let view = configure(Some(&client)).unwrap();
        assert!(view.has_attention_required());
        for (scope, kind) in [
            (HookScope::Ask, AttentionKind::Permission),
            (HookScope::Acp, AttentionKind::Question),
            (HookScope::Subagent, AttentionKind::Permission),
            (HookScope::Interactive, AttentionKind::Permission),
            (HookScope::Interactive, AttentionKind::Question),
            (HookScope::Interactive, AttentionKind::RouteRecovery),
        ] {
            view.run_attention_required(&AttentionRequiredInput {
                invocation: HookInvocation {
                    scope,
                    turn_id: TurnId::new(42),
                },
                kind,
            });
        }
        assert_eq!(
            *client.0.lock().unwrap(),
            [
                ("blocked", Some(b"permission".to_vec())),
                ("blocked", Some(b"question".to_vec())),
                ("blocked", Some(b"recovery".to_vec())),
            ]
        );
    }

    #[test]
    fn disabled_herdr_registers_no_lifecycle_hooks() {
        assert!(
            !configure::<Recording>(None)
                .unwrap()
                .has_attention_required()
        );
    }
}
