use std::sync::Arc;

use ofx_contract::{ChatMessage, TurnId, UiEvent};
use tokio_util::sync::CancellationToken;

use super::{Agent, EventSink, Stop};
use crate::execution_memory::steering_message;
use crate::worker_runtime::{Boundary, BoundaryKind, Steering, WorkerRuntime};

impl Agent {
    #[must_use]
    pub fn with_steering(mut self, worker: Arc<WorkerRuntime>) -> Self {
        self.steering = Some(worker);
        self
    }

    pub(super) fn turn_message(&self, prompt: &str) -> ChatMessage {
        let continues = self
            .steering
            .as_ref()
            .is_some_and(|worker| worker.continues_steering());
        ChatMessage::user(if continues {
            steering_message(prompt)
        } else {
            prompt.to_owned()
        })
    }

    pub(super) fn steering_boundary(&self, kind: BoundaryKind) -> Boundary {
        match &self.steering {
            Some(worker) => worker.take_boundary(kind),
            None if kind == BoundaryKind::Cancelled => Boundary::Interrupt,
            None => Boundary::None,
        }
    }

    fn interrupt_requested(&self, cancel: &CancellationToken) -> bool {
        cancel.is_cancelled()
            || self
                .steering
                .as_ref()
                .is_some_and(|worker| worker.interrupt_requested())
    }

    fn model_step(&self, cancel: &CancellationToken) -> CancellationToken {
        self.steering
            .as_ref()
            .map_or_else(|| cancel.clone(), |worker| worker.model_step(cancel))
    }

    pub(super) fn enter_tool_phase(&self) {
        if let Some(worker) = &self.steering {
            worker.enter_tool_phase();
        }
    }

    pub(super) fn set_compacting(&self, compacting: bool) {
        if let Some(worker) = &self.steering {
            worker.set_compacting(compacting);
        }
    }

    pub(super) fn begin_model_step(
        &mut self,
        turn_id: TurnId,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<CancellationToken, Stop> {
        if self.interrupt_requested(cancel) && !self.steer_after_cancel(turn_id, "", events) {
            return Err(Stop::interrupted());
        }
        if !self.steer_at_model_boundary(turn_id, events) {
            return Err(Stop::interrupted());
        }
        Ok(self.model_step(cancel))
    }

    pub(super) fn steered_after_reply(
        &mut self,
        turn_id: TurnId,
        content: Option<&str>,
        step_cancel: &CancellationToken,
        cancel: &CancellationToken,
        events: EventSink<'_>,
    ) -> Result<bool, Stop> {
        if !step_cancel.is_cancelled() || cancel.is_cancelled() {
            return Ok(false);
        }
        let partial = content.unwrap_or_default();
        if self.steer_after_cancel(turn_id, partial, events) {
            return Ok(true);
        }
        Err(Stop::Interrupted {
            partial: partial.to_owned(),
        })
    }

    pub(super) fn steer_after_cancel(
        &mut self,
        turn_id: TurnId,
        partial: &str,
        events: EventSink<'_>,
    ) -> bool {
        let Boundary::Continue(steering) = self.steering_boundary(BoundaryKind::Cancelled) else {
            return false;
        };
        if !partial.is_empty() {
            self.history.push(ChatMessage::Assistant {
                content: Some(partial.to_owned()),
                tool_calls: Vec::new(),
                provider_replay: None,
            });
        }
        self.append_steering(turn_id, steering, events);
        true
    }

    fn steer_at_model_boundary(&mut self, turn_id: TurnId, events: EventSink<'_>) -> bool {
        match self.steering_boundary(BoundaryKind::Model) {
            Boundary::Continue(steering) => {
                self.append_steering(turn_id, steering, events);
                true
            }
            Boundary::Handoff => false,
            Boundary::None | Boundary::Interrupt => true,
        }
    }

    pub(super) fn finalizing_steering(&self) -> Option<Vec<Steering>> {
        match self.steering_boundary(BoundaryKind::Finalizing) {
            Boundary::Continue(steering) => Some(steering),
            Boundary::None | Boundary::Handoff | Boundary::Interrupt => None,
        }
    }

    pub(super) fn append_steering(
        &mut self,
        turn_id: TurnId,
        steering: Vec<Steering>,
        events: EventSink<'_>,
    ) {
        for Steering { id, text } in steering {
            self.history
                .push(ChatMessage::user(steering_message(&text)));
            events(UiEvent::SteeringApplied {
                turn_id,
                prompt: id,
                text,
            });
        }
    }
}
