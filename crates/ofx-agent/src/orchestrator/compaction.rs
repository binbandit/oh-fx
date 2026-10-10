use std::mem;

use ofx_contract::{
    ChatMessage, CompactionActivity, CompactionEnd, ModelRequest, ProviderError, ProviderErrorKind,
    ProviderOptions, TurnId, UiEvent,
};
use ofx_trace::TraceContext;
use tokio_util::sync::CancellationToken;

use super::{Agent, EventSink, LastReply, Stop, Turn, TurnFailure};
use crate::compactor::trace::{CompactionTraceKind, Optional};
use crate::compactor::{
    self, Compacted, CompactionError, Correction, Size, Step, Summarizer, Tracer,
};
use crate::execution_memory::{history_turns, retain};
use crate::gateway_step::Meter;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Manual,
    Automatic,
    ProviderOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Preparation,
    Summary,
    Publication,
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
    counted_after: bool,
    compacted_len: Option<usize>,
    pub(super) compacted_steps: bool,
}

impl TurnCompaction {
    pub(super) fn checkpointed(&self) -> bool {
        self.compacted_len.is_some()
    }
}

struct Compacting<'r> {
    trace: Tracer,
    size: Size,
    active: bool,
    options: ProviderOptions<'r>,
    conversation: Option<ModelRequest<'r>>,
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
        self.close_interrupted_turns(false);
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
        let trace = self.tracer(TraceContext {
            turn_id: ofx_trace::next_turn_id(),
            ..TraceContext::default()
        });
        let mut stage = Stage::Preparation;
        let mut progress = |step| {
            if step == Step::Summarizing {
                stage = Stage::Summary;
                summarizing();
            }
        };
        let compacting = Compacting {
            trace,
            size,
            active: false,
            options,
            conversation: None,
        };
        let compacted = self
            .compacted_history(compacting, &mut progress, cancel)
            .await
            .inspect_err(|error| trace_failed(trace, stage, Origin::Manual, error.code()))?;
        let Some(compacted) = compacted else {
            trace_nothing_to_compact(trace, Origin::Manual);
            return Ok(Compaction::Unchanged);
        };
        self.record_compaction(None, &compacted)
            .map_err(|failure| {
                trace_failed(trace, Stage::Publication, Origin::Manual, &failure.code);
                CompactionError::NotSaved
            })?;
        self.trace_committed(trace, Origin::Manual, &compacted);
        self.install_compaction(compacted);
        Ok(Compaction::Compacted)
    }

    fn tracer(&self, context: TraceContext) -> Tracer {
        Tracer::new(self.compaction_trace, context)
    }

    fn trace_committed(&self, trace: Tracer, origin: Origin, compacted: &Compacted) {
        trace.info(
            CompactionTraceKind::Committed,
            format_args!(
                "origin={} removed_turns={} compaction_count={} summary_bytes={} tools={}",
                origin.name(),
                compacted.cut.turns,
                self.compactions + 1,
                compacted.text.len(),
                compacted.payload.tool_count,
            ),
        );
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
        let measure = |body: &str, has_images: bool| {
            let cost = RequestCost::measure(body, has_images);
            calibration.map_or(cost, |calibration| cost.calibrated(calibration))
        };
        let body = self.provider.request_body(request)?;
        let has_images = request.messages.iter().any(
            |message| matches!(message, ChatMessage::User { images, .. } if !images.is_empty()),
        );
        let cost = measure(&body, has_images);
        let fixed = ModelRequest {
            messages: &[],
            ..*request
        };
        let fixed_tokens = self
            .provider
            .request_body(&fixed)
            .map(|body| measure(&body, false).estimated_tokens);
        Some((Measured { cost, fixed_tokens }, body))
    }

    pub(super) async fn preflight(
        &self,
        turn: &mut Turn,
        request: ModelRequest<'_>,
        measured: Option<&Measured>,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<Option<Compacted>, CompactionError> {
        let mut shown = CompactionShown::new(turn.id, events);
        let trace = self.tracer(turn.trace);
        let turn = &mut turn.compaction;
        let pending = turn.overflow == Overflow::Pending;
        let rebuilt = mem::take(&mut turn.rebuilt) && !pending;
        let origin = if pending {
            Origin::ProviderOverflow
        } else {
            Origin::Automatic
        };
        if let Some(measured) = measured {
            let mut size = self.compaction_size(measured.fixed_tokens);
            size.request_tokens = Some(measured.cost.estimated_tokens);
            size.overflow = pending;
            if rebuilt {
                trace.log(
                    false,
                    format_args!(
                        "request after compaction estimated_tokens={} fixed_tokens={} usable_tokens={} after_tokens={}",
                        measured.cost.estimated_tokens,
                        Optional(size.fixed_tokens),
                        Optional(size.usable_tokens),
                        size.after_tokens(),
                    ),
                );
            }
            let wants = !rebuilt && (pending || size.due());
            self.trace_decision(trace, wants, pending, measured, size);
            if wants {
                self.set_compacting(true);
                let compacting = Compacting {
                    trace,
                    size,
                    active: true,
                    options: request.provider_options,
                    conversation: (!pending).then_some(request),
                };
                let compacted = self
                    .compacted_history(compacting, &mut |step| shown.step(step), cancel)
                    .await;
                self.set_compacting(false);
                let compacted = compacted.inspect_err(|error| {
                    trace_failed(trace, shown.stage, origin, error.code());
                    shown.failed(*error);
                })?;
                if compacted.is_some() {
                    return Ok(compacted);
                }
                trace_nothing_to_compact(trace, origin);
            } else if rebuilt
                && let Some(usable) = size
                    .usable_tokens
                    .filter(|usable| measured.cost.estimated_tokens > *usable)
            {
                trace.failure(
                    CompactionTraceKind::NoCompactableContext,
                    format_args!(
                        "estimated_tokens={} usable_tokens={usable}",
                        measured.cost.estimated_tokens
                    ),
                );
                return Err(CompactionError::ContextCapacityExceeded);
            }
        }
        if pending {
            trace.failure(
                CompactionTraceKind::OverflowRecoveryIncomplete,
                format_args!(
                    "estimated_tokens={}",
                    measured.map_or(0, |measured| measured.cost.estimated_tokens)
                ),
            );
            return Err(CompactionError::ContextCapacityExceeded);
        }
        Ok(None)
    }

    fn trace_decision(
        &self,
        trace: Tracer,
        wants: bool,
        pending: bool,
        measured: &Measured,
        size: Size,
    ) {
        let prior_input_tokens = self
            .calibration
            .as_ref()
            .filter(|calibration| calibration.model == self.config.model)
            .map(|calibration| calibration.exact_input_tokens);
        trace.info_if(
            wants,
            CompactionTraceKind::Decision,
            format_args!(
                "decision={} overflow={pending} request_bytes={} estimated_tokens={} text_tokens={} has_images=false image_baseline=false prior_input_tokens={} usable_tokens={} compact_at_tokens={} compact_at_percent={} max_output_tokens={}",
                if wants { "compact" } else { "no_op" },
                measured.cost.bytes,
                measured.cost.estimated_tokens,
                measured.cost.text_tokens,
                Optional(prior_input_tokens),
                Optional(size.usable_tokens),
                Optional(size.compact_at_tokens),
                self.config.auto_compact_percent.get(),
                Optional(self.config.max_output_tokens),
            ),
        );
    }

    pub(super) fn adopt_compaction(
        &mut self,
        turn: &mut Turn,
        compacted: Compacted,
        measured: Option<Measured>,
        events: EventSink<'_>,
    ) -> Result<(), Stop> {
        let before = measured.as_ref().map(|measured| measured.cost);
        self.settle_measurement(measured, None);
        let installed = self.install_turn_compaction(turn, compacted, before);
        let activity = if installed.is_ok() {
            CompactionActivity::Compacted
        } else {
            CompactionActivity::Ended(CompactionEnd::Failed)
        };
        events(UiEvent::TurnCompaction {
            turn_id: turn.id,
            activity,
        });
        installed
    }

    fn install_turn_compaction(
        &mut self,
        turn: &mut Turn,
        compacted: Compacted,
        before: Option<RequestCost>,
    ) -> Result<(), Stop> {
        let trace = self.tracer(turn.trace);
        let origin = if turn.compaction.overflow == Overflow::Pending {
            Origin::ProviderOverflow
        } else {
            Origin::Automatic
        };
        self.record_compaction(Some(turn), &compacted)
            .map_err(|failure| {
                trace_failed(trace, Stage::Publication, origin, &failure.code);
                Stop::failed(TurnFailure::Persistence(failure))
            })?;
        self.trace_committed(trace, origin, &compacted);
        let summary_bytes = compacted.text.len();
        let tools = compacted.payload.tool_count;
        let active = self.turn_starts.len().saturating_sub(1);
        let splits_active = compacted.cut.turns == active && compacted.cut.splits_turn();
        if splits_active {
            self.keep_compacted_files(turn, compacted.cut.tool_steps);
        }
        turn.compaction.compacted_steps |= splits_active;
        self.install_compaction(compacted);
        turn.start = self.turn_starts.last().copied().unwrap_or(turn.start);
        turn.compaction.compacted_len = Some(self.history.len());
        turn.compaction.rebuilt = true;
        turn.compaction.counted_after = true;
        if turn.compaction.overflow == Overflow::Pending {
            turn.compaction.overflow = Overflow::Used;
        }
        trace.info(
            CompactionTraceKind::Installed,
            format_args!(
                "request_bytes_before={} estimated_tokens_before={} summary_bytes={summary_bytes} tools={tools}",
                before.map_or(0, |cost| cost.bytes),
                before.map_or(0, |cost| cost.estimated_tokens),
            ),
        );
        Ok(())
    }

    pub(super) fn recovers_overflow(
        &self,
        turn: &mut Turn,
        error: &ProviderError,
        partial: &str,
        measured: Option<&Measured>,
        cancel: &CancellationToken,
    ) -> bool {
        let recovers = partial.is_empty()
            && !cancel.is_cancelled()
            && turn.compaction.overflow == Overflow::Ready
            && self.has_compactable_context(turn)
            && is_context_overflow(error);
        if recovers {
            self.tracer(turn.trace).info(
                CompactionTraceKind::ProviderOverflowRecovery,
                format_args!(
                    "model={} request_bytes={} estimated_tokens={}",
                    self.config.model,
                    measured.map_or(0, |measured| measured.cost.bytes),
                    measured.map_or(0, |measured| measured.cost.estimated_tokens)
                ),
            );
            turn.compaction.overflow = Overflow::Pending;
        }
        recovers
    }

    pub(super) fn trace_request_after_compaction(
        &self,
        turn: &mut Turn,
        measured: Option<&Measured>,
        input_tokens: Option<u64>,
    ) {
        if let (Some(measured), Some(exact)) = (measured, input_tokens)
            && mem::take(&mut turn.compaction.counted_after)
        {
            self.tracer(turn.trace).log(
                false,
                format_args!(
                    "request after compaction exact_input_tokens={exact} estimated_tokens={}",
                    measured.cost.estimated_tokens
                ),
            );
        }
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
            .filter(|calibration| {
                calibration.model == self.config.model
                    && calibration.request.image_identity.is_none()
            })
            .map(|calibration| Correction {
                estimated: calibration.request.text_tokens,
                measured: calibration.exact_input_tokens,
            });
        size
    }

    async fn compacted_history(
        &self,
        compacting: Compacting<'_>,
        progress: &mut (dyn FnMut(Step) + Send),
        cancel: &CancellationToken,
    ) -> Result<Option<Compacted>, CompactionError> {
        let Compacting {
            trace,
            size,
            active,
            options,
            conversation,
        } = compacting;
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
            trace,
            meter: Meter::new(self.network_calls, trace.context()),
        };
        let request = compactor::Request {
            turns: &turns,
            active,
            earlier: self.compacted.as_ref(),
            size,
            model: &self.config.model,
            sends_after_conversation: conversation.is_some(),
            follows_checkpoint: self.compactions > 0,
            trace,
        };
        let compacted = compactor::compact(request, &mut summarizer, progress, cancel).await?;
        if compacted.is_some() && cancel.is_cancelled() {
            return Err(CompactionError::Cancelled);
        }
        Ok(compacted)
    }

    fn install_compaction(&mut self, compacted: Compacted) {
        let pending_turns: Vec<_> = self
            .pending_interruptions
            .iter()
            .filter_map(|range| {
                self.turn_starts
                    .iter()
                    .position(|start| *start == range.start)
            })
            .collect();
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
        self.pending_interruptions = pending_turns
            .into_iter()
            .filter_map(|turn| turn.checked_sub(compacted.cut.turns))
            .filter_map(|turn| {
                let start = *self.turn_starts.get(turn)?;
                let end = self
                    .turn_starts
                    .get(turn + 1)
                    .copied()
                    .unwrap_or(self.history.len());
                Some(start..end)
            })
            .collect();
        self.compacted = Some(compacted.payload);
        self.compactions += 1;
        self.calibration = None;
    }
}

fn trace_failed(trace: Tracer, stage: Stage, origin: Origin, error: &str) {
    let detail = format_args!(
        "stage={} origin={} err={error}",
        stage.name(),
        origin.name()
    );
    if error == CompactionError::Cancelled.code() {
        trace.info(CompactionTraceKind::TransactionFailed, detail);
    } else {
        trace.failure(CompactionTraceKind::TransactionFailed, detail);
    }
}

fn trace_nothing_to_compact(trace: Tracer, origin: Origin) {
    trace.info(
        CompactionTraceKind::Decision,
        format_args!(
            "decision=no_op origin={} reason=nothing_to_compact",
            origin.name()
        ),
    );
}

impl Origin {
    const fn name(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Automatic => "automatic",
            Self::ProviderOverflow => "provider_overflow",
        }
    }
}

impl Stage {
    const fn name(self) -> &'static str {
        match self {
            Self::Preparation => "preparation",
            Self::Summary => "summary",
            Self::Publication => "publication",
        }
    }
}

struct CompactionShown<'e> {
    turn_id: TurnId,
    events: EventSink<'e>,
    started: bool,
    stage: Stage,
}

impl<'e> CompactionShown<'e> {
    fn new(turn_id: TurnId, events: EventSink<'e>) -> Self {
        Self {
            turn_id,
            events,
            started: false,
            stage: Stage::Preparation,
        }
    }

    fn step(&mut self, step: Step) {
        self.started |= step == Step::Chosen;
        if step == Step::Summarizing {
            self.stage = Stage::Summary;
        }
        self.show(match step {
            Step::Chosen => CompactionActivity::Preparing,
            Step::Summarizing => CompactionActivity::Summarizing,
        });
    }

    fn failed(&mut self, error: CompactionError) {
        if self.started {
            self.show(CompactionActivity::Ended(match error {
                CompactionError::Cancelled => CompactionEnd::Cancelled,
                CompactionError::ContextCapacityExceeded => CompactionEnd::ContextTooLarge,
                _ => CompactionEnd::Failed,
            }));
        }
    }

    fn show(&mut self, activity: CompactionActivity) {
        (self.events)(UiEvent::TurnCompaction {
            turn_id: self.turn_id,
            activity,
        });
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
