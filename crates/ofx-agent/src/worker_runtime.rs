use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use ofx_contract::SkillBinding;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Ordinary,
    ActiveTurn,
    Continuation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPrompt {
    pub id: u64,
    pub text: String,
    pub skills: Vec<SkillBinding>,
    delivery: Delivery,
}

impl QueuedPrompt {
    pub fn new(id: u64, text: String, skills: Vec<SkillBinding>) -> Self {
        Self {
            id,
            text,
            skills,
            delivery: Delivery::Ordinary,
        }
    }

    pub fn is_continuation(&self) -> bool {
        self.delivery == Delivery::Continuation
    }

    fn same_turn_eligible(&self) -> bool {
        self.skills.is_empty()
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
        self.queue
            .drain(..reachable)
            .map(|prompt| Steering {
                id: prompt.id,
                text: prompt.text,
            })
            .collect()
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
        let eligible = prompt.same_turn_eligible();
        state.queue.push_back(prompt);
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
        for prompt in &mut state.queue {
            if prompt.delivery == Delivery::ActiveTurn {
                prompt.delivery = Delivery::Continuation;
            }
        }
        state.begin(Work::Idle);
    }

    pub fn request_cancel(&self) {
        self.lock().interruption = Interruption::Stop;
    }

    pub fn clear(&self) {
        self.lock().queue.clear();
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
        state.queue.remove(newest)
    }

    pub(crate) fn take_boundary(&self, kind: BoundaryKind) -> Boundary {
        let mut state = self.lock();
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

#[cfg(test)]
mod tests;
