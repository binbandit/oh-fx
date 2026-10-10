use std::sync::Arc;

use ofx_contract::{AttentionKind, HookRegistrationError, HookRuntime, HookScope, HookView};

use crate::herdr::{Herdr, Reporter, State};

pub(crate) fn configure<Client: Reporter + 'static>(
    herdr: Option<&Arc<Client>>,
) -> Result<HookView, HookRegistrationError> {
    let mut runtime = HookRuntime::default();
    if let Some(herdr) = herdr {
        register(&mut runtime, herdr)?;
    }
    Ok(runtime.freeze())
}

pub(crate) async fn announce(herdr: Option<&Arc<Herdr>>, session: Option<String>) {
    offload(herdr, move |herdr| herdr.initialize(session.as_deref())).await;
}

pub(crate) async fn report_working(herdr: Option<&Arc<Herdr>>) {
    offload(herdr, |herdr| herdr.report(State::Working, None)).await;
}

async fn offload(herdr: Option<&Arc<Herdr>>, report: impl FnOnce(&Herdr) + Send + 'static) {
    if let Some(herdr) = herdr {
        let herdr = Arc::clone(herdr);
        let _ = tokio::task::spawn_blocking(move || report(&herdr)).await;
    }
}

fn register<Client: Reporter + 'static>(
    runtime: &mut HookRuntime,
    herdr: &Arc<Client>,
) -> Result<(), HookRegistrationError> {
    let idle = Arc::clone(herdr);
    runtime.register_post_turn_end("fx.herdr.turn_end", move |input| {
        if input.invocation.scope == HookScope::Interactive {
            idle.report(State::Idle, None);
        }
    })?;
    let blocked = Arc::clone(herdr);
    runtime.register_attention_required("fx.herdr.attention_required", move |input| {
        if input.invocation.scope == HookScope::Interactive {
            blocked.report(State::Blocked, Some(attention_status(input.kind)));
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

    use ofx_contract::{
        AttentionKind, AttentionRequiredInput, HookInvocation, HookScope, PostTurnEndInput, TurnId,
        TurnPresentationOutcome,
    };

    use super::configure;
    use crate::herdr::{Reporter, State};

    type Report = (&'static str, Option<Vec<u8>>);

    #[derive(Default)]
    struct Recording(Mutex<Vec<Report>>);

    impl Reporter for Recording {
        fn report(&self, state: State, status: Option<&[u8]>) {
            let state = match state {
                State::Idle => "idle",
                State::Working => "working",
                State::Blocked => "blocked",
            };
            self.0
                .lock()
                .unwrap()
                .push((state, status.map(<[u8]>::to_vec)));
        }
    }

    fn invocation(scope: HookScope) -> HookInvocation {
        HookInvocation {
            scope,
            turn_id: TurnId::new(42),
        }
    }

    #[test]
    fn herdr_hooks_report_only_interactive_turn_ends_and_attention() {
        let client = Arc::new(Recording::default());
        let view = configure(Some(&client)).unwrap();
        assert!(view.has_post_turn_end());
        assert!(view.has_attention_required());
        for (scope, outcome) in [
            (HookScope::Ask, TurnPresentationOutcome::Completed),
            (HookScope::Subagent, TurnPresentationOutcome::Completed),
            (HookScope::Interactive, TurnPresentationOutcome::Interrupted),
        ] {
            view.run_post_turn_end(&PostTurnEndInput {
                invocation: invocation(scope),
                outcome,
            });
        }
        for (scope, kind) in [
            (HookScope::Ask, AttentionKind::Permission),
            (HookScope::Acp, AttentionKind::Question),
            (HookScope::Subagent, AttentionKind::Permission),
            (HookScope::Interactive, AttentionKind::Permission),
            (HookScope::Interactive, AttentionKind::Question),
            (HookScope::Interactive, AttentionKind::RouteRecovery),
        ] {
            view.run_attention_required(&AttentionRequiredInput {
                invocation: invocation(scope),
                kind,
            });
        }
        assert_eq!(
            *client.0.lock().unwrap(),
            [
                ("idle", None),
                ("blocked", Some(b"permission".to_vec())),
                ("blocked", Some(b"question".to_vec())),
                ("blocked", Some(b"recovery".to_vec())),
            ]
        );
    }

    #[test]
    fn disabled_herdr_registers_no_lifecycle_hooks() {
        let view = configure::<Recording>(None).unwrap();
        assert!(!view.has_post_turn_end());
        assert!(!view.has_attention_required());
    }
}
