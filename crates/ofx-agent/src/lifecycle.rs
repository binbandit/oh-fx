use ofx_contract::{
    HookInvocation, HookScope, HookView, PostTurnEndInput, TurnId, TurnPresentationOutcome,
};

pub(crate) struct LifecycleContext {
    view: HookView,
    scope: HookScope,
}

impl LifecycleContext {
    pub(crate) fn new(view: HookView, scope: HookScope) -> Self {
        Self { view, scope }
    }

    pub(crate) async fn post_turn_end(&self, turn_id: TurnId, outcome: TurnPresentationOutcome) {
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
        let _ = tokio::task::spawn_blocking(move || view.run_post_turn_end(&input)).await;
    }
}
