use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use ofx_contract::{ReasoningEffort, RecoveredTurn, SkillBinding};
use ofx_trace::{TraceContext, trace_event, trace_log};
use tokio_util::sync::CancellationToken;

mod worker_trace;

const WORKER: &str = "worker";
const INTERRUPT: &str = "interrupt";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Ordinary,
    ActiveTurn,
    Continuation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnSettings {
    fast_mode: bool,
    effort: String,
}

impl Default for TurnSettings {
    fn default() -> Self {
        Self {
            fast_mode: false,
            effort: ReasoningEffort::Auto.label().to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPrompt {
    pub id: u64,
    pub text: String,
    pub skills: Vec<SkillBinding>,
    delivery: Delivery,
    recovered: Option<RecoveredTurn>,
    turn_id: u64,
    settings: TurnSettings,
}

impl QueuedPrompt {
    pub fn new(id: u64, text: String, skills: Vec<SkillBinding>) -> Self {
        Self {
            id,
            text,
            skills,
            delivery: Delivery::Ordinary,
            recovered: None,
            turn_id: 0,
            settings: TurnSettings::default(),
        }
    }

    #[must_use]
    pub fn with_settings(mut self, fast_mode: bool, effort: &ReasoningEffort) -> Self {
        self.settings = TurnSettings {
            fast_mode,
            effort: effort.label().to_owned(),
        };
        self
    }

    pub fn turn_id(&self) -> u64 {
        self.turn_id
    }

    pub fn recovery(id: u64, recovered: RecoveredTurn) -> Self {
        Self {
            text: recovered.prompt.clone(),
            recovered: Some(recovered),
            ..Self::new(id, String::new(), Vec::new())
        }
    }

    pub fn recovered(&self) -> Option<&RecoveredTurn> {
        self.recovered.as_ref()
    }

    pub fn is_continuation(&self) -> bool {
        self.delivery == Delivery::Continuation
    }

    fn same_turn_eligible(&self) -> bool {
        self.skills.is_empty() && self.recovered.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundaryKind {
    Model,
    Cancelled,
    Finalizing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Steering {
    pub(crate) id: u64,
    pub(crate) text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Boundary {
    None,
    Continue(Vec<Steering>),
    Handoff,
    Interrupt,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Work {
    #[default]
    Idle,
    Turn {
        continuation: bool,
    },
    Compaction,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Interruption {
    #[default]
    None,
    Steering,
    Stop,
}

#[derive(Debug, Default)]
struct State {
    queue: VecDeque<QueuedPrompt>,
    work: Work,
    tool_phase: bool,
    compacting: bool,
    interruption: Interruption,
    model_step: Option<CancellationToken>,
    active_turn: u64,
}

impl State {
    fn processing(&self) -> bool {
        self.work != Work::Idle
    }

    fn waits_for_boundary(&self) -> bool {
        self.tool_phase || self.compacting || self.work == Work::Compaction
    }

    fn begin(&mut self, work: Work) {
        self.work = work;
        self.tool_phase = false;
        self.compacting = false;
        self.interruption = Interruption::None;
        self.model_step = None;
    }

    fn reachable_steering(&self) -> usize {
        self.queue
            .iter()
            .take_while(|prompt| prompt.delivery == Delivery::ActiveTurn)
            .count()
    }

    fn hands_off(&self) -> bool {
        self.queue
            .iter()
            .take(self.reachable_steering())
            .any(|prompt| !prompt.same_turn_eligible())
    }

    fn take_steering(&mut self) -> Vec<Steering> {
        if self.hands_off() {
            return Vec::new();
        }
        let reachable = self.reachable_steering();
        let steering: Vec<Steering> = self
            .queue
            .drain(..reachable)
            .map(|prompt| Steering {
                id: prompt.id,
                text: prompt.text,
            })
            .collect();
        if !steering.is_empty() {
            worker_trace::steering_consumed(self.active_turn, steering.len());
        }
        steering
    }

    fn compaction_active(&self) -> bool {
        self.compacting || self.work == Work::Compaction
    }
}

#[derive(Debug, Default)]
pub struct WorkerRuntime {
    state: Mutex<State>,
}

impl WorkerRuntime {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn admit(&self, mut prompt: QueuedPrompt) {
        let mut state = self.lock();
        let mut interrupt = false;
        prompt.delivery = if state.processing() && state.interruption != Interruption::Stop {
            interrupt =
                !state.waits_for_boundary() && state.reachable_steering() == state.queue.len();
            Delivery::ActiveTurn
        } else {
            Delivery::Ordinary
        };
        if prompt.turn_id == 0 {
            prompt.turn_id = ofx_trace::next_turn_id();
        }
        let eligible = prompt.same_turn_eligible();
        let admitted = worker_trace::Admitted {
            turn_id: prompt.turn_id,
            prompt_bytes: prompt.text.len(),
            targeted: prompt.delivery == Delivery::ActiveTurn,
            plain: eligible,
            interrupt_model: interrupt,
        };
        let settings = prompt.settings.clone();
        state.queue.push_back(prompt);
        worker_trace::enqueued(&admitted, state.queue.len(), &settings);
        if state.processing() {
            let active = (
                state.active_turn,
                state.tool_phase,
                state.compaction_active(),
            );
            worker_trace::steering_admission(&admitted, active);
        }
        if interrupt {
            state.interruption = if eligible {
                Interruption::Steering
            } else {
                Interruption::Stop
            };
            if let Some(step) = &state.model_step {
                step.cancel();
            }
        }
    }

    pub fn take_next(&self) -> Option<QueuedPrompt> {
        let mut state = self.lock();
        let prompt = state.queue.pop_front()?;
        let continuation = prompt.is_continuation();
        state.begin(Work::Turn { continuation });
        state.active_turn = prompt.turn_id;
        worker_trace::began(&prompt, state.queue.len());
        if continuation {
            for queued in &mut state.queue {
                if !queued.is_continuation() || !queued.same_turn_eligible() {
                    break;
                }
                queued.delivery = Delivery::ActiveTurn;
            }
        }
        Some(prompt)
    }

    pub fn begin_compaction(&self) {
        self.lock().begin(Work::Compaction);
    }

    pub fn finish_processing(&self) {
        let mut state = self.lock();
        trace_log!(WORKER, "finish processing queued={}", state.queue.len());
        let finished = state.active_turn;
        for prompt in &mut state.queue {
            if prompt.delivery == Delivery::ActiveTurn {
                prompt.delivery = Delivery::Continuation;
                worker_trace::steering_deferred(finished, prompt);
            }
        }
        state.begin(Work::Idle);
        state.active_turn = 0;
    }

    pub fn request_cancel(&self) {
        let mut state = self.lock();
        let processing = state.processing();
        let queued = state.queue.len();
        trace_log!(
            WORKER,
            "cancel requested processing={processing} queued={queued}"
        );
        trace_event!(
            INTERRUPT,
            "cancel_requested",
            TraceContext::default(),
            "processing={processing} queued={queued} active_tool_known=false"
        );
        state.interruption = Interruption::Stop;
    }

    pub fn request_interactive_cancel(&self) {
        let mut state = self.lock();
        if state.interruption == Interruption::Stop {
            return;
        }
        let processing = state.processing();
        let queued = state.queue.len();
        let steering_pending = state
            .queue
            .iter()
            .any(|prompt| prompt.delivery != Delivery::Ordinary);
        trace_log!(
            WORKER,
            "cancel requested processing={processing} queued={queued} steering_pending={steering_pending}"
        );
        trace_event!(
            INTERRUPT,
            "cancel_requested",
            TraceContext::default(),
            "processing={processing} queued={queued} steering_pending={steering_pending} active_tool_known=false"
        );
        state.interruption = Interruption::Stop;
    }

    pub fn clear(&self) {
        let mut state = self.lock();
        let dropped = state.queue.len();
        if dropped > 0 {
            trace_log!(WORKER, "clear queued prompts dropped={dropped}");
            trace_event!(
                WORKER,
                "queued_prompts_cleared",
                TraceContext::default(),
                "dropped={dropped}"
            );
        }
        state.queue.clear();
    }

    pub fn holds_recovery(&self) -> bool {
        self.lock()
            .queue
            .iter()
            .any(|prompt| prompt.recovered.is_some())
    }

    pub fn discard_before(&self, first_kept: u64) {
        let mut state = self.lock();
        let (kept, removed): (VecDeque<_>, VecDeque<_>) = state
            .queue
            .drain(..)
            .partition(|prompt| prompt.id >= first_kept);
        state.queue = kept;
        for (index, prompt) in removed.iter().enumerate() {
            worker_trace::removed(
                prompt.turn_id,
                removed.len() - index - 1 + state.queue.len(),
            );
        }
    }

    pub fn has_waiting_prompts(&self) -> bool {
        !self.lock().queue.is_empty()
    }

    pub fn waiting_texts(&self) -> Vec<String> {
        self.lock()
            .queue
            .iter()
            .map(|prompt| prompt.text.clone())
            .collect()
    }

    pub fn pop_queued_steer_for_edit(&self) -> Option<QueuedPrompt> {
        let mut state = self.lock();
        if !state.processing()
            || !state.waits_for_boundary()
            || state.interruption != Interruption::None
        {
            return None;
        }
        let newest = state.queue.iter().rposition(|prompt| {
            prompt.delivery == Delivery::ActiveTurn && prompt.same_turn_eligible()
        })?;
        let retracted = state.queue.remove(newest)?;
        worker_trace::retracted(retracted.turn_id, state.queue.len());
        Some(retracted)
    }

    pub(crate) fn take_boundary(&self, kind: BoundaryKind) -> Boundary {
        let mut state = self.lock();
        let boundary = boundary(&mut state, kind);
        worker_trace::boundary_checked(state.active_turn, kind, &boundary);
        boundary
    }

    pub(crate) fn model_step(&self, turn: &CancellationToken) -> CancellationToken {
        let mut state = self.lock();
        let step = turn.child_token();
        if state.interruption != Interruption::None {
            step.cancel();
        }
        state.tool_phase = false;
        state.model_step = Some(step.clone());
        step
    }

    pub(crate) fn enter_tool_phase(&self) {
        let mut state = self.lock();
        state.tool_phase = true;
        state.model_step = None;
    }

    pub(crate) fn set_compacting(&self, compacting: bool) {
        self.lock().compacting = compacting;
    }

    pub(crate) fn continues_steering(&self) -> bool {
        self.lock().work == Work::Turn { continuation: true }
    }

    pub(crate) fn interrupt_requested(&self) -> bool {
        self.lock().interruption != Interruption::None
    }

    pub(crate) fn steering_interrupt(&self) -> bool {
        self.lock().interruption == Interruption::Steering
    }
}

fn boundary(state: &mut State, kind: BoundaryKind) -> Boundary {
    if !state.processing() {
        return if kind == BoundaryKind::Cancelled && state.interruption != Interruption::None {
            Boundary::Interrupt
        } else {
            Boundary::None
        };
    }
    match kind {
        BoundaryKind::Cancelled => match state.interruption {
            Interruption::None => Boundary::None,
            Interruption::Stop => Boundary::Interrupt,
            Interruption::Steering => {
                let steering = state.take_steering();
                if steering.is_empty() {
                    return Boundary::Interrupt;
                }
                state.interruption = Interruption::None;
                Boundary::Continue(steering)
            }
        },
        BoundaryKind::Model | BoundaryKind::Finalizing => {
            if state.interruption != Interruption::None {
                return Boundary::None;
            }
            if state.hands_off() {
                return Boundary::Handoff;
            }
            let steering = state.take_steering();
            if steering.is_empty() {
                Boundary::None
            } else {
                Boundary::Continue(steering)
            }
        }
    }
}

#[cfg(test)]
mod tests;
