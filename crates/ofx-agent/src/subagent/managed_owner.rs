use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_contract::{
    ApprovalOrigin, ApprovalRequest, ChildKind, ChildPhase, ChildSnapshot, LivePermissionMode,
    ModelFailureDiagnostic, RootUserRequests, SubagentPlan, SubagentRequest, SubagentStatus,
};
use tokio::sync::watch;
use tokio_util::sync::{CancellationToken, DropGuard};

use super::child_state::{ActiveWork, Child, Outcome, Registry};
use super::execution::{ChildRuntime, WorkOutcome};
use super::tool_host::{ChildAgents, ChildDefaults, effective_settings};

type SharedRuntime = Arc<tokio::sync::Mutex<ChildRuntime>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Observation {
    pub(crate) outcome: Option<Outcome>,
    pub(crate) failure: Option<ModelFailureDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finished {
    pub(crate) observation: Observation,
    pub(crate) text: Option<String>,
}

pub(crate) enum Admitted {
    Ready(Waiter),
    Completed(Finished),
    Rejected(&'static str),
}

pub(crate) struct Waiter {
    abandoned: DropGuard,
    finished: watch::Receiver<Option<Finished>>,
    status: SubagentStatus,
}

impl Waiter {
    pub(crate) fn status(&self) -> &SubagentStatus {
        &self.status
    }
}

pub(crate) enum Observed {
    Finished(Finished),
    Cancelled,
    Unavailable,
}

struct Slot {
    cancel: CancellationToken,
    finished: watch::Receiver<Option<Finished>>,
    status: SubagentStatus,
}

impl Slot {
    fn waiter(&self) -> Waiter {
        Waiter {
            abandoned: self.cancel.clone().drop_guard(),
            finished: self.finished.clone(),
            status: self.status.clone(),
        }
    }
}

struct NamedChild {
    runtime: SharedRuntime,
    status: SubagentStatus,
}

#[derive(Default)]
struct State {
    registry: Registry,
    named: HashMap<String, NamedChild>,
    slots: HashMap<String, Slot>,
    texts: HashMap<String, Option<String>>,
    issued: u64,
}

struct Start {
    child_id: String,
    work: ActiveWork,
    instructions: String,
    runtime: SharedRuntime,
    status: SubagentStatus,
}

pub(crate) struct Owner {
    agents: Arc<dyn ChildAgents>,
    state: Mutex<State>,
}

impl Owner {
    pub(crate) fn new(agents: Arc<dyn ChildAgents>) -> Self {
        Self {
            agents,
            state: Mutex::default(),
        }
    }

    pub(crate) fn admit(
        self: &Arc<Self>,
        request: &SubagentRequest,
        operation_id: &str,
        root_user_requests: Arc<RootUserRequests>,
    ) -> Admitted {
        let defaults = self.agents.defaults();
        let fingerprint = request.fingerprint();
        let mut guard = self.lock();
        let state = &mut *guard;
        if let Some(existing) = state.registry.find_by_operation(operation_id) {
            if existing.operation_fingerprint(operation_id) != Some(fingerprint) {
                return Admitted::Rejected("operation_conflict");
            }
            if existing.last_work_id.as_deref() == Some(operation_id) {
                return Admitted::Completed(Finished {
                    observation: observation(existing),
                    text: state.texts.get(&existing.id).cloned().flatten(),
                });
            }
            return state
                .slots
                .get(&existing.id)
                .map_or(Admitted::Rejected("state_unavailable"), |slot| {
                    Admitted::Ready(slot.waiter())
                });
        }
        let work = ActiveWork {
            id: operation_id.to_owned(),
            request_fingerprint: fingerprint,
            message: request.content().to_owned(),
            root_user_requests,
            permission_mode: defaults.permission_mode,
        };
        let start = match request.agent_name() {
            None => self.create(state, request, &defaults, work),
            Some(agent) => match state
                .registry
                .find_persistent(agent)
                .map(|child| (child.id.clone(), child.phase))
            {
                Some((child_id, phase)) => {
                    continue_persistent(state, request, agent, (&child_id, phase), work)
                }
                None => self.create(state, request, &defaults, work),
            },
        };
        match start {
            Ok(start) => Admitted::Ready(self.start(state, start)),
            Err(code) => Admitted::Rejected(code),
        }
    }

    pub(crate) fn clear(&self) {
        let mut state = self.lock();
        for slot in state.slots.values() {
            slot.cancel.cancel();
        }
        *state = State {
            issued: state.issued,
            ..State::default()
        };
    }

    pub(crate) async fn observe(waiter: Waiter, parent: &CancellationToken) -> Observed {
        let Waiter {
            abandoned,
            mut finished,
            ..
        } = waiter;
        let observed = tokio::select! {
            biased;
            () = parent.cancelled() => return Observed::Cancelled,
            value = finished.wait_for(Option::is_some) => match value {
                Ok(value) => value.clone().map_or(Observed::Unavailable, Observed::Finished),
                Err(_) => Observed::Unavailable,
            },
        };
        abandoned.disarm();
        observed
    }

    fn create(
        &self,
        state: &mut State,
        request: &SubagentRequest,
        defaults: &ChildDefaults,
        work: ActiveWork,
    ) -> Result<Start, &'static str> {
        state.issued += 1;
        let child_id = state.issued.to_string();
        let instructions = request.instructions().unwrap_or_default();
        match request.agent_name() {
            None => state.registry.append_one_off(&child_id, work.clone()),
            Some(agent) => {
                state
                    .registry
                    .append_persistent(&child_id, agent, instructions, work.clone())
            }
        }
        .map_err(|_| "host_failure")?;
        let settings = effective_settings(&defaults.settings, request.overrides());
        let permission_mode = LivePermissionMode::from(defaults.permission_mode);
        let agent = self.agents.agent(&settings, permission_mode.clone());
        let runtime = Arc::new(tokio::sync::Mutex::new(ChildRuntime::new(
            agent,
            permission_mode,
        )));
        let status = SubagentStatus {
            model: settings.model,
            effort: settings.effort,
        };
        if request.agent_name().is_some() {
            state.named.insert(
                child_id.clone(),
                NamedChild {
                    runtime: Arc::clone(&runtime),
                    status: status.clone(),
                },
            );
        }
        Ok(Start {
            child_id,
            work,
            instructions: instructions.to_owned(),
            runtime,
            status,
        })
    }

    fn start(self: &Arc<Self>, state: &mut State, start: Start) -> Waiter {
        let (sender, receiver) = watch::channel(None);
        let cancel = CancellationToken::new();
        let slot = Slot {
            cancel: cancel.clone(),
            finished: receiver,
            status: start.status.clone(),
        };
        let waiter = slot.waiter();
        state.slots.insert(start.child_id.clone(), slot);
        let owner = Arc::clone(self);
        tokio::spawn(async move {
            let Start {
                child_id,
                work,
                instructions,
                runtime,
                ..
            } = start;
            let active = work.clone();
            let agents = Arc::clone(&owner.agents);
            let origin = child_id.clone();
            let run = tokio::spawn(async move {
                let forward = |request: ApprovalRequest| {
                    agents.approval_requested(ApprovalRequest {
                        origin: ApprovalOrigin::Subagent(origin.clone()),
                        ..request
                    });
                };
                let mut runtime = runtime.lock_owned().await;
                let tools = agents.work_tools();
                runtime
                    .run(&active, &instructions, tools, &forward, &cancel)
                    .await
            });
            let outcome = run.await.unwrap_or_else(|_| WorkOutcome::panicked());
            owner.finish(&child_id, &work.id, outcome, &sender);
        });
        waiter
    }

    fn finish(
        &self,
        child_id: &str,
        work_id: &str,
        outcome: WorkOutcome,
        sender: &watch::Sender<Option<Finished>>,
    ) {
        let mut state = self.lock();
        state.slots.remove(child_id);
        let Ok(child) = state
            .registry
            .finish(child_id, work_id, outcome.outcome, outcome.failure)
        else {
            return;
        };
        let observation = observation(child);
        state
            .texts
            .insert(child_id.to_owned(), outcome.text.clone());
        drop(state);
        sender.send_replace(Some(Finished {
            observation,
            text: outcome.text,
        }));
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn continue_persistent(
    state: &mut State,
    request: &SubagentRequest,
    agent: &str,
    (child_id, phase): (&str, ChildPhase),
    work: ActiveWork,
) -> Result<Start, &'static str> {
    if request.overrides().is_present() {
        return Err("override_after_create");
    }
    match request.plan(Some(ChildSnapshot {
        kind: ChildKind::Persistent,
        phase,
    })) {
        SubagentPlan::ContinuePersistent => {}
        SubagentPlan::SteerPersistent => return Err("child_busy"),
        SubagentPlan::Reject(code) => return Err(code.code()),
        SubagentPlan::CreateOneOff | SubagentPlan::CreatePersistent => return Err("host_failure"),
    }
    let (runtime, status) = state
        .named
        .get(child_id)
        .map(|named| (Arc::clone(&named.runtime), named.status.clone()))
        .ok_or("state_unavailable")?;
    let instructions = state
        .registry
        .start_persistent_work(agent, request.instructions(), work.clone())
        .map_err(|_| "host_failure")?
        .instructions()
        .to_owned();
    Ok(Start {
        child_id: child_id.to_owned(),
        work,
        instructions,
        runtime,
        status,
    })
}

fn observation(child: &Child) -> Observation {
    Observation {
        outcome: child.last_outcome,
        failure: child.last_failure.clone(),
    }
}
