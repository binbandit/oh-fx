use std::sync::Arc;

use ofx_contract::{
    ApprovalRequest, LivePermissionMode, LogFailure, ModelFailureDiagnostic, RestoredHistory,
    TurnOutcome, UiEvent,
};
use ofx_text::is_terminal_safe;
use tokio_util::sync::CancellationToken;

use super::child_state::{ActiveWork, Outcome};
use super::tool_host::{ChildRecord, WorkTools};
use crate::orchestrator::{Agent, TurnFailure, TurnReport};

const MAX_DIAGNOSTIC_BYTES: usize = 256;

pub(crate) struct ChildRelay<'a> {
    pub(crate) approvals: &'a (dyn Fn(ApprovalRequest) -> Result<(), LogFailure> + Sync),
    pub(crate) feedback: &'a (dyn Fn(String) + Sync),
}

pub(crate) struct ChildRuntime {
    agent: Agent,
    base_prompt: String,
    permission_mode: LivePermissionMode,
    record: Option<Arc<dyn ChildRecord>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkOutcome {
    pub(crate) outcome: Outcome,
    pub(crate) failure: Option<ModelFailureDiagnostic>,
    pub(crate) text: Option<String>,
}

impl WorkOutcome {
    pub(crate) fn panicked() -> Self {
        Self {
            outcome: Outcome::Failed,
            failure: Some(failure_diagnostic_value("agent_execution", "Panicked")),
            text: None,
        }
    }
}

impl ChildRuntime {
    pub(crate) fn new(agent: Agent, permission_mode: LivePermissionMode) -> Self {
        Self {
            base_prompt: agent.config().system_prompt.clone(),
            agent,
            permission_mode,
            record: None,
        }
    }

    pub(crate) fn restored(mut self, history: RestoredHistory) -> Self {
        self.agent.restore(history);
        self
    }

    pub(crate) fn saved(mut self, child_id: &str, record: Arc<dyn ChildRecord>) -> Self {
        self.agent.attach_session(child_id.to_owned(), record.log());
        self.record = Some(record);
        self
    }

    pub(crate) async fn run(
        &mut self,
        work: &ActiveWork,
        instructions: &str,
        tools: WorkTools,
        relay: &ChildRelay<'_>,
        cancel: &CancellationToken,
    ) -> WorkOutcome {
        let WorkTools { tools, release } = tools;
        if let Some(record) = &self.record {
            record.begin_work(&work.id);
        }
        self.agent.replace_tools(tools);
        let mut config = self.agent.config().clone();
        config.system_prompt = system_prompt(&self.base_prompt, instructions);
        self.agent.set_config(config);
        self.agent
            .inherit_root_user_requests(Arc::clone(&work.root_user_requests));
        self.permission_mode.set(work.permission_mode);
        let mut partial = String::new();
        let mut unsaved = None;
        let stop = cancel.child_token();
        let report = self
            .agent
            .run_turn(
                &work.message,
                &mut |event| match event {
                    UiEvent::AssistantText { text, .. }
                    | UiEvent::AssistantRestarted { text, .. } => partial.push_str(&text),
                    UiEvent::ToolStarted { .. } | UiEvent::ToolRejected { .. } => partial.clear(),
                    UiEvent::ApprovalRequested { request, .. } => {
                        if let Err(failure) = (relay.approvals)(*request) {
                            unsaved.get_or_insert(failure);
                            stop.cancel();
                        }
                    }
                    UiEvent::ApprovalFeedback { text, .. } => (relay.feedback)(text),
                    _ => {}
                },
                &stop,
            )
            .await;
        self.agent.replace_tools(Vec::new());
        release.await;
        match unsaved {
            Some(failure) if !cancel.is_cancelled() => WorkOutcome {
                outcome: Outcome::Failed,
                failure: Some(turn_failure_diagnostic(Some(&TurnFailure::Persistence(
                    failure,
                )))),
                text: (!partial.is_empty()).then_some(partial),
            },
            _ => work_outcome(report, partial, cancel.is_cancelled()),
        }
    }
}

pub(crate) fn system_prompt(base: &str, instructions: &str) -> String {
    if instructions.is_empty() {
        base.to_owned()
    } else {
        format!("{base}\n\n<subagent_instructions>\n{instructions}\n</subagent_instructions>")
    }
}

fn work_outcome(report: TurnReport, partial: String, cancelled: bool) -> WorkOutcome {
    let unsaved = matches!(report.failure, Some(TurnFailure::Persistence(_)));
    let outcome = if cancelled {
        Outcome::Cancelled
    } else if unsaved {
        Outcome::Failed
    } else {
        match report.outcome {
            TurnOutcome::Completed => Outcome::Completed,
            TurnOutcome::Interrupted => Outcome::Interrupted,
            TurnOutcome::Failed => Outcome::Failed,
        }
    };
    let failure =
        (outcome == Outcome::Failed).then(|| turn_failure_diagnostic(report.failure.as_ref()));
    let text = match report.outcome {
        TurnOutcome::Completed if unsaved => {
            (!report.final_text.is_empty()).then_some(report.final_text)
        }
        TurnOutcome::Completed => Some(report.final_text),
        TurnOutcome::Interrupted | TurnOutcome::Failed => (!partial.is_empty()).then_some(partial),
    };
    WorkOutcome {
        outcome,
        failure,
        text,
    }
}

fn turn_failure_diagnostic(failure: Option<&TurnFailure>) -> ModelFailureDiagnostic {
    match failure {
        Some(TurnFailure::Provider(error)) if error.status.is_some() => {
            let title = if matches!(error.status, Some(401 | 403)) {
                "API access denied"
            } else {
                "API request failed"
            };
            let detail = error.diagnostic.as_deref().unwrap_or(&error.code);
            failure_diagnostic_value("provider_http_error", &format!("{title} · {detail}"))
        }
        Some(failure) => failure_diagnostic_value("agent_turn_failed", failure.code()),
        None => failure_diagnostic_value("agent_execution", "ProviderFailed"),
    }
}

pub(crate) fn failure_diagnostic_value(code: &str, detail: &str) -> ModelFailureDiagnostic {
    let prefix = &code[..code.floor_char_boundary(MAX_DIAGNOSTIC_BYTES)];
    let room = MAX_DIAGNOSTIC_BYTES
        .saturating_sub(prefix.len())
        .saturating_sub(2);
    let detail = if is_terminal_safe(detail.as_bytes()) {
        &detail[..detail.floor_char_boundary(room)]
    } else {
        ""
    };
    if detail.is_empty() {
        ModelFailureDiagnostic::new(prefix)
    } else {
        ModelFailureDiagnostic::new(&format!("{prefix}: {detail}"))
    }
}

#[cfg(test)]
mod tests;
