use ofx_contract::{
    HookInvocation, HookScope, HookView, PostTurnEndInput, TurnId, TurnPresentationOutcome,
};
use tokio::task::JoinHandle;

pub(crate) struct LifecycleContext {
    view: HookView,
    scope: HookScope,
    pending: Option<JoinHandle<()>>,
}

impl LifecycleContext {
    pub(crate) fn new(view: HookView, scope: HookScope) -> Self {
        Self {
            view,
            scope,
            pending: None,
        }
    }

    pub(crate) fn post_turn_end(&mut self, turn_id: TurnId, outcome: TurnPresentationOutcome) {
        if !self.view.has_post_turn_end() {
            return;
        }
        let view = self.view.clone();
        let input = PostTurnEndInput {
            invocation: HookInvocation {
                scope: self.scope,
                turn_id,
            },
            outcome,
        };
        self.pending = Some(tokio::task::spawn_blocking(move || {
            view.run_post_turn_end(&input);
        }));
    }

    pub(crate) async fn settle(&mut self) {
        if let Some(pending) = self.pending.take() {
            let _ = pending.await;
        }
    }
}
