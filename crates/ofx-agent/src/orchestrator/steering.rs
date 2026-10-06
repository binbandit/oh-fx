use std::sync::Arc;

use ofx_contract::{ChatMessage, TurnId, UiEvent};
use tokio_util::sync::CancellationToken;

use super::{Agent, EventSink, Stop, Turn};
use crate::execution_memory::{steering_message, steering_text};
use crate::worker_runtime::{Boundary, BoundaryKind, Steering, WorkerRuntime};

impl Agent {
    pub(super) fn close_interrupted_turns(&mut self, continuation: bool) {
        let mut trailing = if continuation
            && self
                .pending_interruptions
                .last()
                .is_some_and(|range| range.end == self.history.len())
        {
            self.pending_interruptions.pop()
        } else {
            None
        };
        while let Some(range) = self.pending_interruptions.pop() {
            let end = range.end;
            let added = crate::execution_memory::close_interrupted_turn(&mut self.history, range);
            for start in &mut self.turn_starts {
                if *start >= end {
                    *start += added;
                }
            }
            if let Some(range) = &mut trailing {
                range.start += added;
                range.end += added;
            }
        }
        self.pending_interruptions.extend(trailing);
    }

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
        turn: &mut Turn,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<CancellationToken, Stop> {
        if self.interrupt_requested(cancel) && !self.steer_after_cancel(turn.id, cancel, events) {
            return Err(Stop::interrupted());
        }
        if !self.steer_at_model_boundary(turn.id, events) {
            return Err(Stop::interrupted());
        }
        self.follow_steered_language(turn);
        Ok(self.model_step(cancel))
    }

    pub(super) fn steers_after_interrupt(&self, cancel: &CancellationToken) -> bool {
        !cancel.is_cancelled()
            && self
                .steering
                .as_ref()
                .is_some_and(|worker| worker.steering_interrupt())
    }

    pub(super) fn keep_interrupted_reply(&mut self, partial: &str) {
        if !partial.is_empty() {
            self.history.push(ChatMessage::Assistant {
                content: Some(partial.to_owned()),
                tool_calls: Vec::new(),
                provider_replay: None,
            });
        }
    }

    pub(super) fn steered_after_reply(
        &mut self,
        content: Option<&str>,
        step_cancel: &CancellationToken,
        cancel: &CancellationToken,
    ) -> Result<bool, Stop> {
        if !step_cancel.is_cancelled() || cancel.is_cancelled() {
            return Ok(false);
        }
        let partial = content.unwrap_or_default();
        if !self.steers_after_interrupt(cancel) {
            return Err(Stop::Interrupted {
                partial: partial.to_owned(),
            });
        }
        self.keep_interrupted_reply(partial);
        Ok(true)
    }

    fn steer_after_cancel(
        &mut self,
        turn_id: TurnId,
        cancel: &CancellationToken,
        events: EventSink<'_>,
    ) -> bool {
        if cancel.is_cancelled() {
            return false;
        }
        let Boundary::Continue(steering) = self.steering_boundary(BoundaryKind::Cancelled) else {
            return false;
        };
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

    pub(super) fn settle_steering(&mut self, start: usize) {
        for (index, message) in self.history.iter_mut().enumerate().skip(start) {
            let ChatMessage::User {
                content,
                restored_steering: false,
                feedback_for: None,
            } = message
            else {
                continue;
            };
            let Some(text) = steering_text(content) else {
                continue;
            };
            *message = if index == start {
                ChatMessage::user(text)
            } else {
                ChatMessage::restored_steering(text)
            };
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
