use std::mem;

use ofx_contract::{
    ApplicableTarget, CallDescription, ChatMessage, Concurrency, PreparedCall, TargetKind,
    ToolCall, ToolDeferral, ToolEffect, ToolRejection, ToolResultStatus, TurnId, UiEvent,
};
use tokio_util::sync::CancellationToken;

use super::{
    Agent, EventSink, Prepared, Rejection, Stop, ToolOutput, TurnFailure, completed, contained,
    discard,
};

pub(super) const CONTEXT_DEFERRED_OUTPUT: &str = "Scoped project instructions were added before execution. Review them and reissue this tool call if it is still appropriate.";
pub(super) const NOT_EXECUTED_OUTPUT: &str = "Not executed";

pub(super) struct ProjectGate {
    calls: Vec<GatedCall>,
    delta: bool,
}

enum GatedCall {
    Terminal(Prepared),
    Candidate(Candidate),
    Released,
}

struct Candidate {
    prepared: Box<dyn PreparedCall>,
    description: CallDescription,
    target: Option<ApplicableTarget>,
    mutates: bool,
    deferred: bool,
}

pub(super) enum GatedGroup<'c> {
    Run(Vec<(&'c ToolCall, Prepared)>),
    Unexecuted(CallDescription, &'static str),
}

impl Drop for ProjectGate {
    fn drop(&mut self) {
        mem::take(&mut self.calls).into_iter().for_each(discard);
    }
}

impl GatedCall {
    fn is_parallel(&self) -> bool {
        match self {
            Self::Terminal(prepared) => prepared.is_parallel(),
            Self::Candidate(candidate) => {
                candidate.description.concurrency == Concurrency::Parallel
            }
            Self::Released => false,
        }
    }

    fn target(&self) -> Option<&ApplicableTarget> {
        match self {
            Self::Candidate(candidate) => candidate.target.as_ref(),
            Self::Terminal(_) | Self::Released => None,
        }
    }

    fn release(&mut self, tool_name: &str) -> Prepared {
        match mem::replace(self, Self::Released) {
            Self::Terminal(prepared) => prepared,
            Self::Candidate(candidate) => completed(candidate.prepared, tool_name),
            Self::Released => Prepared::Rejected(Rejection::panicked(tool_name)),
        }
    }
}

impl Agent {
    pub(super) fn open_gate(
        &mut self,
        turn_id: TurnId,
        calls: &[ToolCall],
        malformed: &mut [Option<ToolOutput>],
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<ProjectGate, Stop> {
        let mut gate = ProjectGate {
            calls: calls
                .iter()
                .zip(malformed)
                .map(|(call, malformed)| self.gated_call(call, malformed.take()))
                .collect(),
            delta: false,
        };
        let targets: Vec<ApplicableTarget> = gate
            .calls
            .iter()
            .filter_map(GatedCall::target)
            .cloned()
            .collect();
        let Some(project) = self.project.as_ref().filter(|_| !targets.is_empty()) else {
            return Ok(gate);
        };
        let select = |targets: &[ApplicableTarget]| {
            contained(|| project.provider.select(targets, &project.delivery))
        };
        let Some(selected) = select(&targets) else {
            return Err(failed_gate(turn_id, calls, events));
        };
        for text in &selected.notices {
            events(UiEvent::ContextNotice {
                turn_id,
                text: text.clone(),
            });
        }
        if selected.content.is_some() {
            gate.delta = true;
            let lone = gate.calls.len() == 1;
            for call in &mut gate.calls {
                let GatedCall::Candidate(candidate) = call else {
                    continue;
                };
                if candidate.description.effect == ToolEffect::ReadOnly {
                    continue;
                }
                candidate.deferred = match (lone, &candidate.target) {
                    (true, _) => true,
                    (false, Some(target)) => match select(std::slice::from_ref(target)) {
                        Some(own) => own.content.is_some(),
                        None => return Err(failed_gate(turn_id, calls, events)),
                    },
                    (false, None) => false,
                };
            }
        }
        if cancel.is_cancelled() {
            return Err(Stop::interrupted());
        }
        if let Some(project) = &mut self.project {
            project.delivery.commit(&selected);
            project.deltas.extend(selected.content);
        }
        Ok(gate)
    }

    fn gated_call(&self, call: &ToolCall, malformed: Option<ToolOutput>) -> GatedCall {
        let (prepared, description, mutates) = match self.prepare_uncompleted(call, malformed) {
            Prepared::Ready(prepared, description, mutation, _)
                if description.effect != ToolEffect::None =>
            {
                (prepared, description, mutation.is_some())
            }
            terminal => return GatedCall::Terminal(terminal),
        };
        match self.projected_target(call, prepared.as_ref()) {
            Ok(target) => GatedCall::Candidate(Candidate {
                prepared,
                description,
                target,
                mutates,
                deferred: false,
            }),
            Err(panicked) => {
                discard(prepared);
                GatedCall::Terminal(Prepared::Rejected(panicked))
            }
        }
    }

    fn projected_target(
        &self,
        call: &ToolCall,
        prepared: &dyn PreparedCall,
    ) -> Result<Option<ApplicableTarget>, Rejection> {
        let own = contained(|| prepared.applicable_target())
            .ok_or_else(|| Rejection::panicked(&call.name))?;
        Ok(own.or_else(|| self.permissions.applicable_target(call)))
    }

    pub(super) fn gated_group<'c>(
        &self,
        gate: &mut ProjectGate,
        calls: &'c [ToolCall],
        start: usize,
    ) -> GatedGroup<'c> {
        let mut end = start + 1;
        if !gate.delta && gate.calls[start].is_parallel() {
            while end < calls.len() && gate.calls[end].is_parallel() {
                end += 1;
            }
        }
        if end > start + 1 && (start..end).all(|index| self.is_fresh(gate, calls, index)) {
            return GatedGroup::Run(
                (start..end)
                    .map(|index| (&calls[index], gate.calls[index].release(&calls[index].name)))
                    .collect(),
            );
        }
        let call = &calls[start];
        let candidate = match mem::replace(&mut gate.calls[start], GatedCall::Released) {
            GatedCall::Candidate(candidate) => candidate,
            mut settled => return GatedGroup::Run(vec![(call, settled.release(&call.name))]),
        };
        let checked = if candidate.deferred {
            Err((candidate.prepared, CONTEXT_DEFERRED_OUTPUT))
        } else if candidate.mutates {
            completed_mutation(call, candidate.prepared, candidate.target)
        } else {
            match self.projected_target(call, candidate.prepared.as_ref()) {
                Ok(current) if current == candidate.target => {
                    Ok(completed(candidate.prepared, &call.name))
                }
                Ok(_) => Err((candidate.prepared, NOT_EXECUTED_OUTPUT)),
                Err(panicked) => {
                    discard(candidate.prepared);
                    Ok(Prepared::Rejected(panicked))
                }
            }
        };
        match checked {
            Ok(prepared) => GatedGroup::Run(vec![(call, prepared)]),
            Err((prepared, output)) => {
                GatedGroup::Unexecuted(untargeted(prepared, candidate.description), output)
            }
        }
    }

    fn is_fresh(&self, gate: &ProjectGate, calls: &[ToolCall], index: usize) -> bool {
        let GatedCall::Candidate(candidate) = &gate.calls[index] else {
            return true;
        };
        self.projected_target(&calls[index], candidate.prepared.as_ref())
            .is_ok_and(|current| current == candidate.target)
    }

    pub(super) fn settle_unexecuted(
        &mut self,
        turn_id: TurnId,
        call: &ToolCall,
        description: CallDescription,
        output: &str,
        events: EventSink<'_>,
    ) {
        events(UiEvent::ToolStarted {
            turn_id,
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            description,
        });
        let deferral = if output == CONTEXT_DEFERRED_OUTPUT {
            ToolDeferral::ProjectInstructions
        } else {
            ToolDeferral::TargetChanged
        };
        events(UiEvent::ToolDeferred {
            turn_id,
            call_id: call.id.clone(),
            deferral,
        });
        self.history.push(ChatMessage::Tool {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: output.to_owned(),
            status: ToolResultStatus::Failure,
        });
    }
}

fn completed_mutation(
    call: &ToolCall,
    prepared: Box<dyn PreparedCall>,
    target: Option<ApplicableTarget>,
) -> Result<Prepared, (Box<dyn PreparedCall>, &'static str)> {
    let Some(target) = target else {
        return Err((prepared, NOT_EXECUTED_OUTPUT));
    };
    let (prepared, description, mutation, command) = match completed(prepared, &call.name) {
        Prepared::Ready(prepared, description, mutation, command) => {
            (prepared, description, mutation, command)
        }
        rejected @ Prepared::Rejected(_) => return Ok(rejected),
    };
    let fresh = if let Some(mutation) = &mutation {
        target.kind == TargetKind::File && mutation.target == target.path
    } else {
        let Some(current) = contained(|| prepared.applicable_target()) else {
            discard(prepared);
            return Ok(Prepared::Rejected(Rejection::panicked(&call.name)));
        };
        current == Some(target)
    };
    if fresh {
        Ok(Prepared::Ready(prepared, description, mutation, command))
    } else {
        Err((prepared, NOT_EXECUTED_OUTPUT))
    }
}

fn untargeted(
    prepared: Box<dyn PreparedCall>,
    mut description: CallDescription,
) -> CallDescription {
    if let Some(label) = contained(|| prepared.untargeted_label()).flatten() {
        description.relabel(label);
    }
    discard(prepared);
    description
}

fn failed_gate(turn_id: TurnId, calls: &[ToolCall], events: EventSink<'_>) -> Stop {
    for call in calls {
        events(UiEvent::ToolRejected {
            turn_id,
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            arguments: call.arguments.clone(),
            reason: ToolRejection::Panicked,
            description: None,
        });
    }
    Stop::failed(TurnFailure::ProjectContext)
}
