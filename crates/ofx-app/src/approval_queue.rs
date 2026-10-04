use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_agent::Approvals;
use ofx_contract::{ApprovalAnswer, ApprovalDecision, ApprovalRequest, RequestId, TurnId, UiEvent};

use crate::app_agent_runtime::Emit;

#[derive(Default)]
pub(crate) struct ApprovalQueue {
    approvals: Approvals,
    state: Mutex<QueueState>,
}

#[derive(Default)]
struct QueueState {
    emit: Option<Emit>,
    turn: Option<TurnId>,
    shown: Option<RequestId>,
    waiting: VecDeque<(TurnId, ApprovalRequest)>,
}

struct Shown {
    emit: Option<Emit>,
    event: UiEvent,
}

impl ApprovalQueue {
    pub(crate) fn shared() -> Arc<Self> {
        let queue = Arc::new(Self::default());
        let watched = Arc::downgrade(&queue);
        queue.approvals.on_withdrawn(move |id| {
            if let Some(queue) = watched.upgrade() {
                queue.withdrawn(id);
            }
        });
        queue
    }

    pub(crate) fn approvals(&self) -> &Approvals {
        &self.approvals
    }

    pub(crate) fn attach(&self, emit: Emit) {
        self.lock().emit = Some(emit);
    }

    pub(crate) fn turn_started(&self, turn_id: TurnId) {
        let mut state = self.lock();
        state.turn = Some(turn_id);
        state.shown = None;
    }

    pub(crate) fn turn_finished(&self) {
        let mut state = self.lock();
        state.turn = None;
        state.shown = None;
        let waiting = std::mem::take(&mut state.waiting);
        drop(state);
        for (_, request) in waiting {
            self.approvals.resolve(request.id, ApprovalDecision::Deny);
        }
    }

    pub(crate) fn own(&self, turn_id: TurnId, request: ApprovalRequest) {
        let mut state = self.lock();
        if state.shown.is_some() {
            state.waiting.push_front((turn_id, request));
            return;
        }
        let shown = state.show(turn_id, request);
        drop(state);
        shown.emit();
    }

    pub(crate) fn child(&self, origin: Option<TurnId>, request: ApprovalRequest) {
        let mut state = self.lock();
        let Some(turn_id) = state.turn.filter(|turn| origin == Some(*turn)) else {
            drop(state);
            self.approvals.resolve(request.id, ApprovalDecision::Deny);
            return;
        };
        if state.shown.is_some() {
            state.waiting.push_back((turn_id, request));
            return;
        }
        let shown = state.show(turn_id, request);
        drop(state);
        shown.emit();
    }

    pub(crate) fn child_feedback(&self, origin: Option<TurnId>, text: String) {
        let state = self.lock();
        let Some(turn_id) = state.turn.filter(|turn| origin == Some(*turn)) else {
            return;
        };
        let emit = state.emit.clone();
        drop(state);
        if let Some(emit) = emit {
            emit(UiEvent::ApprovalFeedback { turn_id, text });
        }
    }

    pub(crate) fn resolve(&self, id: RequestId, answer: impl Into<ApprovalAnswer>) {
        self.approvals.resolve(id, answer);
        self.retire(id);
    }

    pub(crate) fn withdrawn(&self, id: RequestId) {
        self.retire(id);
    }

    fn retire(&self, id: RequestId) {
        let mut state = self.lock();
        if state.shown != Some(id) {
            state.waiting.retain(|(_, request)| request.id != id);
            return;
        }
        state.shown = None;
        let Some((turn_id, request)) = state.waiting.pop_front() else {
            return;
        };
        let shown = state.show(turn_id, request);
        drop(state);
        shown.emit();
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl QueueState {
    fn show(&mut self, turn_id: TurnId, request: ApprovalRequest) -> Shown {
        self.shown = Some(request.id);
        Shown {
            emit: self.emit.clone(),
            event: UiEvent::ApprovalRequested {
                turn_id,
                request: Box::new(request),
            },
        }
    }
}

impl Shown {
    fn emit(self) {
        if let Some(emit) = self.emit {
            emit(self.event);
        }
    }
}

#[cfg(test)]
mod tests;
