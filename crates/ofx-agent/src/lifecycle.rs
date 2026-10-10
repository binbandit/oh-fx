use ofx_contract::{
    HookInvocation, HookScope, HookView, PostTurnEndInput, PreToolUseInput, PreToolUseOutcome,
    ToolCall, TurnId, TurnPresentationOutcome, pre_tool_use_blocked_json,
    pre_tool_use_failed_closed_json,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolPreparation {
    Unchanged,
    Rewritten(String),
    Blocked(String),
}

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

    pub(crate) fn has_pre_tool_use(&self) -> bool {
        self.view.has_pre_tool_use()
    }

    pub(crate) async fn pre_tool_use(
        &self,
        turn_id: TurnId,
        step_index: usize,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> Option<ToolPreparation> {
        if cancel.is_cancelled() {
            return None;
        }
        let view = self.view.clone();
        let scope = self.scope;
        let call_id = call.id.as_str().to_owned();
        let tool_name = call.name.clone();
        let arguments_json = call.arguments.clone();
        let dispatched = tokio::task::spawn_blocking(move || {
            view.run_pre_tool_use(&PreToolUseInput {
                invocation: HookInvocation { scope, turn_id },
                step_index,
                call_id: &call_id,
                tool_name: &tool_name,
                arguments_json: &arguments_json,
            })
        })
        .await;
        if cancel.is_cancelled() {
            return None;
        }
        Some(match dispatched {
            Ok(Ok(PreToolUseOutcome::Unchanged)) => ToolPreparation::Unchanged,
            Ok(Ok(PreToolUseOutcome::Rewritten(arguments))) => {
                ToolPreparation::Rewritten(arguments)
            }
            Ok(Ok(PreToolUseOutcome::Blocked(reason))) => {
                ToolPreparation::Blocked(pre_tool_use_blocked_json(&call.name, &reason))
            }
            Ok(Err(_)) | Err(_) => {
                ToolPreparation::Blocked(pre_tool_use_failed_closed_json(&call.name))
            }
        })
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
