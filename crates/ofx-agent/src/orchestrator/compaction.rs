use std::mem;

use ofx_contract::{ChatMessage, ModelRequest, ProviderError, ProviderErrorKind, ProviderOptions};
use tokio_util::sync::CancellationToken;

use super::{Agent, LastReply, Stop, Turn, TurnFailure};
use crate::compactor::{self, Compacted, CompactionError, Correction, Size, Summarizer};
use crate::execution_memory::{history_turns, retain};
use crate::prompt_context::{Calibration, RequestCost};

const CONTEXT_LENGTH_EXCEEDED: &str = "context_length_exceeded";
const OVERFLOW_DETAILS: [&str; 8] = [
    CONTEXT_LENGTH_EXCEEDED,
    "exceeds the context window",
    "exceeded the context window",
    "maximum context length",
    "maximum prompt length",
    "input is too long",
    "prompt is too long",
    "too many input tokens",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compaction {
    Compacted,
    Unchanged,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Overflow {
    #[default]
    Ready,
    Pending,
    Used,
}

#[derive(Debug, Default)]
pub(super) struct TurnCompaction {
    overflow: Overflow,
    rebuilt: bool,
    compacted_len: Option<usize>,
    pub(super) compacted_steps: bool,
}

impl TurnCompaction {
    pub(super) fn checkpointed(&self) -> bool {
        self.compacted_len.is_some()
    }
}

pub(super) struct Measured {
    cost: RequestCost,
    fixed_tokens: Option<usize>,
}

impl Agent {
    pub fn has_context_to_compact(&self) -> bool {
        !self.turn_starts.is_empty()
    }

    pub async fn compact(
        &mut self,
        summarizing: &mut (dyn FnMut() + Send),
        cancel: &CancellationToken,
    ) -> Result<Compaction, CompactionError> {
        if self.resolve_capabilities(cancel).await.is_err() {
            return Err(CompactionError::Cancelled);
        }
        let options = self
            .capabilities
            .as_ref()
            .map_or_else(ProviderOptions::default, |known| {
                known.model.provider_options(
                    self.config.reasoning_effort.as_deref(),
                    self.config.fast_mode,
                )
            });
        let size = self.compaction_size(self.request_fixed_tokens);
        let compacted = self
            .compacted_history(size, false, options, None, summarizing, cancel)
            .await?;
        let Some(compacted) = compacted else {
            return Ok(Compaction::Unchanged);
        };
        self.record_compaction(None, &compacted)
            .map_err(|_| CompactionError::NotSaved)?;
        self.install_compaction(compacted);
        Ok(Compaction::Compacted)
    }

    pub(super) fn has_compactable_context(&self, turn: &Turn) -> bool {
        match turn.compaction.compacted_len {
            None => turn.start > 0 || self.history.len() > turn.start + 1,
            Some(len) => self.history.len() > len,
        }
    }

    pub(super) fn measure(
        &self,
        turn: &Turn,
        request: &ModelRequest<'_>,
    ) -> Option<(Measured, String)> {
        let window_known = self
            .capabilities
            .as_ref()
            .is_some_and(|known| known.model.context_window.is_some());
        if !window_known && turn.compaction.overflow != Overflow::Pending {
            return None;
        }
        let calibration = self
            .calibration
            .as_ref()
            .filter(|calibration| calibration.model == request.model);
        let measure = |body: &str| {
            let cost = RequestCost::measure(body);
            calibration.map_or(cost, |calibration| cost.calibrated(calibration))
        };
        let body = self.provider.request_body(request)?;
        let cost = RequestCost::measure(&body);
        let cost = calibration.map_or(cost, |calibration| cost.calibrated(calibration));
        let fixed = ModelRequest {
            messages: &[],
            ..*request
        };
        let fixed_tokens = self
            .provider
            .request_body(&fixed)
            .map(|body| measure(&body).estimated_tokens);
        Some((Measured { cost, fixed_tokens }, body))
    }

    pub(super) async fn preflight(
        &self,
        turn: &mut TurnCompaction,
        request: ModelRequest<'_>,
        measured: Option<&Measured>,
        cancel: &CancellationToken,
    ) -> Result<Option<Compacted>, CompactionError> {
        let pending = turn.overflow == Overflow::Pending;
        let rebuilt = mem::take(&mut turn.rebuilt) && !pending;
        if let Some(measured) = measured {
            let mut size = self.compaction_size(measured.fixed_tokens);
            size.request_tokens = Some(measured.cost.estimated_tokens);
            size.overflow = pending;
            if !rebuilt && (pending || size.due()) {
                let conversation = (!pending).then_some(request);
                let compacted = self
                    .compacted_history(
                        size,
                        true,
                        request.provider_options,
                        conversation,
                        &mut || {},
                        cancel,
                    )
                    .await?;
                if compacted.is_some() {
                    return Ok(compacted);
                }
            } else if rebuilt
                && size
                    .usable_tokens
                    .is_some_and(|usable| measured.cost.estimated_tokens > usable)
            {
                return Err(CompactionError::ContextCapacityExceeded);
            }
        }
        if pending {
            return Err(CompactionError::ContextCapacityExceeded);
        }
        Ok(None)
    }

    pub(super) fn install_turn_compaction(
        &mut self,
        turn: &mut Turn,
        compacted: Compacted,
    ) -> Result<(), Stop> {
        self.record_compaction(Some(turn), &compacted)
            .map_err(|failure| Stop::failed(TurnFailure::Persistence(failure)))?;
        let active = self.turn_starts.len().saturating_sub(1);
        turn.compaction.compacted_steps |=
            compacted.cut.turns == active && compacted.cut.splits_turn();
        self.install_compaction(compacted);
        turn.start = self.turn_starts.last().copied().unwrap_or(turn.start);
        turn.compaction.compacted_len = Some(self.history.len());
        turn.compaction.rebuilt = true;
        if turn.compaction.overflow == Overflow::Pending {
            turn.compaction.overflow = Overflow::Used;
        }
        Ok(())
    }

    pub(super) fn recovers_overflow(
        &self,
        turn: &mut Turn,
        error: &ProviderError,
        partial: &str,
        cancel: &CancellationToken,
    ) -> bool {
        let recovers = partial.is_empty()
            && !cancel.is_cancelled()
            && turn.compaction.overflow == Overflow::Ready
            && self.has_compactable_context(turn)
            && is_context_overflow(error);
        if recovers {
            turn.compaction.overflow = Overflow::Pending;
        }
        recovers
    }

    pub(super) fn settle_measurement(
        &mut self,
        measured: Option<Measured>,
        input_tokens: Option<u64>,
    ) {
        let Some(measured) = measured else {
            return;
        };
        self.request_fixed_tokens = measured.fixed_tokens;
        if let Some(exact) = input_tokens {
            self.calibration = Some(Calibration {
                model: self.config.model.clone(),
                request: measured.cost,
                exact_input_tokens: usize::try_from(exact).unwrap_or(usize::MAX),
            });
        }
    }

    fn compaction_size(&self, fixed_tokens: Option<usize>) -> Size {
        let context_window = self
            .capabilities
            .as_ref()
            .and_then(|known| known.model.context_window);
        let mut size = Size::of(
            context_window,
            self.config.max_output_tokens,
            self.config.auto_compact_percent,
        );
        size.fixed_tokens = fixed_tokens;
        size.correction = self
            .calibration
            .as_ref()
            .filter(|calibration| calibration.model == self.config.model)
            .map(|calibration| Correction {
                estimated: calibration.request.text_tokens,
                measured: calibration.exact_input_tokens,
            });
        size
    }

    async fn compacted_history(
        &self,
        size: Size,
        active: bool,
        options: ProviderOptions<'_>,
        conversation: Option<ModelRequest<'_>>,
        summarizing: &mut (dyn FnMut() + Send),
        cancel: &CancellationToken,
    ) -> Result<Option<Compacted>, CompactionError> {
        let turns = history_turns(&self.history, &self.turn_starts);
        let reasoning_efforts = self
            .capabilities
            .as_ref()
            .map_or(&[][..], |known| &known.model.reasoning_efforts);
        let mut summarizer = Summarizer {
            provider: &*self.provider,
            model: &self.config.model,
            max_output_tokens: self.config.max_output_tokens,
            options,
            reasoning_efforts,
            conversation,
            session_id: self.session_id.as_deref(),
            cancel,
        };
        let request = compactor::Request {
            turns: &turns,
            active,
            earlier: self.compacted.as_ref(),
            size,
            model: &self.config.model,
            sends_after_conversation: conversation.is_some(),
        };
        compactor::compact(request, &mut summarizer, summarizing, cancel).await
    }

    fn install_compaction(&mut self, compacted: Compacted) {
        self.last_reply = self.last_reply.take().and_then(|reply| {
            reply
                .turn
                .checked_sub(compacted.cut.turns)
                .map(|turn| LastReply { turn, ..reply })
        });
        self.ledger.compact(compacted.cut);
        retain(
            &mut self.history,
            &mut self.turn_starts,
            compacted.cut,
            ChatMessage::user(compacted.text),
        );
        self.compacted = Some(compacted.payload);
        self.calibration = None;
    }
}

pub(super) fn compaction_stop(error: CompactionError, cancel: &CancellationToken) -> Stop {
    if error == CompactionError::Cancelled || cancel.is_cancelled() {
        Stop::interrupted()
    } else {
        Stop::failed(TurnFailure::Compaction(error))
    }
}

fn is_context_overflow(error: &ProviderError) -> bool {
    let detail = error.detail.as_deref().map(str::to_ascii_lowercase);
    let mentions = |needles: &[&str]| {
        detail
            .as_deref()
            .is_some_and(|detail| needles.iter().any(|needle| detail.contains(needle)))
    };
    match error.kind {
        ProviderErrorKind::RequestTooLarge => true,
        ProviderErrorKind::InvalidRequest => mentions(&OVERFLOW_DETAILS),
        ProviderErrorKind::ProviderError => {
            error.code.eq_ignore_ascii_case(CONTEXT_LENGTH_EXCEEDED)
                || mentions(&[CONTEXT_LENGTH_EXCEEDED])
        }
        _ => false,
    }
}
