use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use ofx_contract::{
    ApprovalOrigin, ApprovalRequest, ChildKind, ChildPhase, ChildSnapshot, LivePermissionMode,
    LogFailure, ModelFailureDiagnostic, PermissionMode, RootUserRequests, SubagentPlan,
    SubagentRequest, SubagentStatus, TurnId,
};
use tokio::sync::watch;
use tokio_util::sync::{CancellationToken, DropGuard};

use super::child_state::{ActiveWork, Child, Outcome, Registry};
use super::execution::{ChildRuntime, WorkOutcome};
use super::tool_host::{
    ChildAgents, ChildDefaults, ChildSettings, ChildStore, ResumedChild, effective_settings,
};

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
    Failed(String),
}

enum Refusal {
    Code(&'static str),
    Store(String),
}

impl From<&'static str> for Refusal {
    fn from(code: &'static str) -> Self {
        Self::Code(code)
    }
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
    store: Option<Arc<dyn ChildStore>>,
    unavailable: bool,
}

impl State {
    fn save(&self) -> Result<(), LogFailure> {
        match &self.store {
            Some(store) => store.save_registry(&self.registry.render(store.parent_id())),
            None => Ok(()),
        }
    }

    fn restore(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        let restored = store.load_registry().ok().map(|saved| {
            saved.map_or(Ok(Registry::default()), |bytes| {
                Registry::parse(&bytes, store.parent_id())
            })
        });
        match restored {
            Some(Ok(mut registry)) => {
                let interrupted = registry.interrupt_active();
                self.registry = registry;
                if interrupted {
                    let _ = self.save();
                }
            }
            _ => self.unavailable = true,
        }
    }

    fn saved_reply(&self, child_id: &str, work_id: &str) -> Option<String> {
        if let Some(text) = self.texts.get(child_id) {
            return text.clone();
        }
        self.store
            .as_ref()?
            .reply_for_work(child_id, work_id)
            .ok()
            .flatten()
    }
}

struct Start {
    child_id: String,
    work: ActiveWork,
    instructions: String,
    runtime: SharedRuntime,
    status: SubagentStatus,
}

struct Planned {
    child_id: String,
    work: ActiveWork,
    instructions: String,
    runner: Runner,
    status: SubagentStatus,
}

enum Runner {
    Existing(SharedRuntime),
    Fresh(Box<FreshChild>),
}

struct FreshChild {
    runtime: ChildRuntime,
    settings: ChildSettings,
    named: bool,
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
        turn_id: Option<TurnId>,
    ) -> Admitted {
        let defaults = self.agents.defaults();
        let fingerprint = request.fingerprint();
        let mut guard = self.lock();
        let state = &mut *guard;
        if state.unavailable {
            return Admitted::Rejected("host_unavailable");
        }
        if let Some(existing) = state.registry.find_by_operation(operation_id) {
            if existing.operation_fingerprint(operation_id) != Some(fingerprint) {
                return Admitted::Rejected("operation_conflict");
            }
            if existing.last_work_id.as_deref() == Some(operation_id) {
                return Admitted::Completed(Finished {
                    observation: observation(existing),
                    text: state.saved_reply(&existing.id, operation_id),
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
            root_user_context: self.agents.root_user_context(&root_user_requests),
            root_user_requests,
            permission_mode: defaults.permission_mode,
            created_at_ms: now_ms(),
        };
        let before = state.registry.clone();
        let planned = match request.agent_name() {
            None => self.create(state, request, &defaults, work),
            Some(agent) => match state
                .registry
                .find_persistent(agent)
                .map(|child| (child.id.clone(), child.phase))
            {
                Some((child_id, phase)) => continue_persistent(
                    state,
                    self.agents.as_ref(),
                    request,
                    (agent, &child_id, phase),
                    work,
                ),
                None => self.create(state, request, &defaults, work),
            },
        };
        let start = planned.and_then(|planned| publish(state, planned, &before));
        match start {
            Ok(start) => Admitted::Ready(self.start(state, start, turn_id)),
            Err(refusal) => {
                state.registry = before;
                match refusal {
                    Refusal::Code(code) => Admitted::Rejected(code),
                    Refusal::Store(code) => Admitted::Failed(code),
                }
            }
        }
    }

    pub(crate) fn bind(&self, store: Option<Arc<dyn ChildStore>>) {
        let bound = self
            .lock()
            .store
            .as_ref()
            .map(|bound| bound.parent_id().to_owned());
        if bound.as_deref() == store.as_ref().map(|store| store.parent_id()) {
            return;
        }
        self.clear();
        let mut state = self.lock();
        state.store = store;
        state.unavailable = false;
        state.restore();
    }

    pub(crate) fn clear(&self) {
        let mut state = self.lock();
        for slot in state.slots.values() {
            slot.cancel.cancel();
        }
        *state = State {
            issued: state.issued,
            store: state.store.take(),
            unavailable: state.unavailable,
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
    ) -> Result<Planned, Refusal> {
        let child_id = if let Some(store) = &state.store {
            store
                .new_child_id()
                .map_err(|failure| Refusal::Store(failure.code))?
        } else {
            state.issued += 1;
            state.issued.to_string()
        };
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
        let status = SubagentStatus {
            model: settings.model.clone(),
            effort: settings.effort.clone(),
        };
        Ok(Planned {
            child_id,
            work,
            instructions: instructions.to_owned(),
            runner: Runner::Fresh(Box::new(FreshChild {
                runtime: ChildRuntime::new(agent, permission_mode),
                settings,
                named: request.agent_name().is_some(),
            })),
            status,
        })
    }

    fn start(self: &Arc<Self>, state: &mut State, start: Start, turn_id: Option<TurnId>) -> Waiter {
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
            let waiting = Arc::clone(&owner);
            let run = tokio::spawn(async move {
                let forward = |request: ApprovalRequest| {
                    waiting.await_approval(&origin, &active.id)?;
                    agents.approval_requested(
                        turn_id,
                        ApprovalRequest {
                            origin: ApprovalOrigin::Subagent(origin.clone()),
                            ..request
                        },
                    );
                    Ok(())
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
        let before = state.registry.clone();
        let Ok(child) = state
            .registry
            .finish(child_id, work_id, outcome.outcome, outcome.failure)
        else {
            return;
        };
        let observation = observation(child);
        if state.save().is_err() {
            state.registry = before;
            return;
        }
        state
            .texts
            .insert(child_id.to_owned(), outcome.text.clone());
        drop(state);
        sender.send_replace(Some(Finished {
            observation,
            text: outcome.text,
        }));
    }

    fn await_approval(&self, child_id: &str, work_id: &str) -> Result<(), LogFailure> {
        let mut state = self.lock();
        let before = state.registry.clone();
        if state.registry.await_approval(child_id, work_id).is_err() {
            return Ok(());
        }
        let saved = state.save();
        if saved.is_err() {
            state.registry = before;
        }
        saved
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn continue_persistent(
    state: &mut State,
    agents: &dyn ChildAgents,
    request: &SubagentRequest,
    (agent, child_id, phase): (&str, &str, ChildPhase),
    work: ActiveWork,
) -> Result<Planned, Refusal> {
    if request.overrides().is_present() {
        return Err("override_after_create".into());
    }
    match request.plan(Some(ChildSnapshot {
        kind: ChildKind::Persistent,
        phase,
    })) {
        SubagentPlan::ContinuePersistent => {}
        SubagentPlan::SteerPersistent => return Err("child_busy".into()),
        SubagentPlan::Reject(code) => return Err(code.code().into()),
        SubagentPlan::CreateOneOff | SubagentPlan::CreatePersistent => {
            return Err("host_failure".into());
        }
    }
    if !state.named.contains_key(child_id) {
        let named = resume_named(state, agents, child_id, work.permission_mode)?;
        state.named.insert(child_id.to_owned(), named);
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
    Ok(Planned {
        child_id: child_id.to_owned(),
        work,
        instructions,
        runner: Runner::Existing(runtime),
        status,
    })
}

fn publish(state: &mut State, planned: Planned, before: &Registry) -> Result<Start, Refusal> {
    state
        .save()
        .map_err(|failure| Refusal::Store(failure.code))?;
    let opened = open(state, planned);
    if opened.is_err() {
        state.registry = before.clone();
        let _ = state.save();
    }
    opened
}

fn open(state: &mut State, planned: Planned) -> Result<Start, Refusal> {
    let Planned {
        child_id,
        work,
        instructions,
        runner,
        status,
    } = planned;
    let runtime = match runner {
        Runner::Existing(runtime) => runtime,
        Runner::Fresh(fresh) => {
            let FreshChild {
                mut runtime,
                settings,
                named,
            } = *fresh;
            if let Some(store) = &state.store {
                let record = store
                    .start_child(&child_id, &settings)
                    .map_err(|failure| Refusal::Store(failure.code))?;
                runtime = runtime.saved(&child_id, record);
            }
            let runtime = Arc::new(tokio::sync::Mutex::new(runtime));
            if named {
                state.named.insert(
                    child_id.clone(),
                    NamedChild {
                        runtime: Arc::clone(&runtime),
                        status: status.clone(),
                    },
                );
            }
            runtime
        }
    };
    Ok(Start {
        child_id,
        work,
        instructions,
        runtime,
        status,
    })
}

fn resume_named(
    state: &State,
    agents: &dyn ChildAgents,
    child_id: &str,
    permission_mode: PermissionMode,
) -> Result<NamedChild, Refusal> {
    let store = state.store.as_ref().ok_or("state_unavailable")?;
    let ResumedChild {
        record,
        settings,
        history,
    } = store
        .resume_child(child_id)
        .map_err(|failure| Refusal::Store(failure.code))?;
    let permission_mode = LivePermissionMode::from(permission_mode);
    let runtime = ChildRuntime::new(
        agents.agent(&settings, permission_mode.clone()),
        permission_mode,
    )
    .restored(history)
    .saved(child_id, record);
    Ok(NamedChild {
        runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
        status: SubagentStatus {
            model: settings.model,
            effort: settings.effort,
        },
    })
}

fn observation(child: &Child) -> Observation {
    Observation {
        outcome: child.last_outcome,
        failure: child.last_failure.clone(),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}
