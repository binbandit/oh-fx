use std::sync::Arc;

use ofx_contract::{
    ApprovalRequest, BoxFuture, LivePermissionMode, ModelFailureDiagnostic, PermissionMode,
    ReasoningEffort, SubagentOverride, SubagentProvider, SubagentRequest, SubagentResult, Tool,
    ToolContext, ToolOutput, TurnId,
};
use ofx_text::lowercase_hex;
use sha2::{Digest, Sha256};

use super::child_state::Outcome;
use super::managed_owner::{Admitted, Finished, Observation, Observed, Owner};
use crate::orchestrator::Agent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSettings {
    pub model: String,
    pub effort: ReasoningEffort,
    pub fast_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildDefaults {
    pub settings: ChildSettings,
    pub permission_mode: PermissionMode,
}

pub struct WorkTools {
    pub tools: Vec<Arc<dyn Tool>>,
    pub release: BoxFuture<'static, ()>,
}

pub trait ChildAgents: Send + Sync {
    fn defaults(&self) -> ChildDefaults;

    fn agent(&self, settings: &ChildSettings, permission_mode: LivePermissionMode) -> Agent;

    fn work_tools(&self) -> WorkTools;

    fn approval_requested(&self, turn_id: Option<TurnId>, request: ApprovalRequest);
}

pub struct SubagentHost {
    owner: Arc<Owner>,
}

impl SubagentHost {
    pub fn new(agents: Arc<dyn ChildAgents>) -> Self {
        Self {
            owner: Arc::new(Owner::new(agents)),
        }
    }

    pub fn clear(&self) {
        self.owner.clear();
    }
}

impl SubagentProvider for SubagentHost {
    fn execute(
        &self,
        request: SubagentRequest,
        context: ToolContext,
    ) -> BoxFuture<'static, ToolOutput> {
        let owner = Arc::clone(&self.owner);
        Box::pin(async move { execute_managed(&owner, &request, &context).await })
    }
}

async fn execute_managed(
    owner: &Arc<Owner>,
    request: &SubagentRequest,
    context: &ToolContext,
) -> ToolOutput {
    let operation_id = operation_id(context.call_id.as_str());
    let root_user_requests = context.root_user_requests.clone().unwrap_or_default();
    match owner.admit(request, &operation_id, root_user_requests, context.turn_id) {
        Admitted::Rejected(code) => output(SubagentResult::failure(code)),
        Admitted::Completed(finished) => complete(&finished),
        Admitted::Ready(waiter) => match Owner::observe(waiter, &context.cancellation).await {
            Observed::Finished(finished) => complete(&finished),
            Observed::Cancelled => output(SubagentResult::failure("child_cancelled")),
            Observed::Unavailable => output(SubagentResult::failure("state_unavailable")),
        },
    }
}

pub(crate) fn effective_settings(
    defaults: &ChildSettings,
    overrides: SubagentOverride<'_>,
) -> ChildSettings {
    ChildSettings {
        model: overrides.model.unwrap_or(&defaults.model).to_owned(),
        effort: overrides
            .effort
            .cloned()
            .unwrap_or_else(|| defaults.effort.clone()),
        fast_mode: defaults.fast_mode,
    }
}

fn operation_id(invocation_id: &str) -> String {
    let identity = Sha256::new()
        .chain_update(b"model\0")
        .chain_update(invocation_id)
        .finalize();
    let mut epoch = [0; 8];
    epoch.copy_from_slice(&identity[..8]);
    format!(
        "fxop:2:m:{}:{}",
        u64::from_le_bytes(epoch) | 1,
        lowercase_hex(&Sha256::digest(invocation_id))
    )
}

fn complete(finished: &Finished) -> ToolOutput {
    let text = finished.text.as_deref();
    let failure_text = (finished.observation.outcome == Some(Outcome::Failed)).then(|| {
        failed_result(
            finished
                .observation
                .failure
                .as_ref()
                .map(ModelFailureDiagnostic::as_str),
            text,
        )
    });
    output(terminal_result(
        &finished.observation,
        failure_text.as_deref().or(text),
    ))
}

fn output(result: SubagentResult<'_>) -> ToolOutput {
    if result.ok {
        ToolOutput::success(result.encode())
    } else {
        ToolOutput::failure(result.encode())
    }
}

fn failed_result(failure: Option<&str>, partial: Option<&str>) -> String {
    let partial = partial.unwrap_or_default();
    format!(
        "Subagent failed: {}. Earlier tool calls may have completed; their effects are not rolled back.{}{partial}",
        failure.unwrap_or("failure reason unavailable"),
        if partial.is_empty() {
            ""
        } else {
            "\n\nPartial result:\n"
        },
    )
}

fn terminal_result<'a>(observation: &Observation, result: Option<&'a str>) -> SubagentResult<'a> {
    let failed = |code| SubagentResult {
        result,
        ..SubagentResult::failure(code)
    };
    match observation.outcome {
        None => failed("child_result_unavailable"),
        Some(Outcome::Completed) if result.is_some() => SubagentResult {
            ok: true,
            result,
            ..SubagentResult::default()
        },
        Some(Outcome::Completed) => SubagentResult::failure("child_result_unavailable"),
        Some(Outcome::Failed) => failed("child_failed"),
        Some(Outcome::Cancelled) => failed("child_cancelled"),
        Some(Outcome::Interrupted) => failed("child_interrupted"),
    }
}

#[cfg(test)]
mod tests;
