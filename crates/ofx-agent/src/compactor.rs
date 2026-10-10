mod checkpoint;
mod ledger;
mod lint;
mod model;
mod summarize;
pub(crate) mod trace;
mod window;

use std::fmt;

use ofx_contract::BoxFuture;
use ofx_text::StreamingEstimator;
use tokio_util::sync::CancellationToken;

use crate::execution_memory::{Cut, HistoryTurn, Note, ToolStep};

pub use checkpoint::checkpoint_model_text;
pub(crate) use checkpoint::{Payload, encode_checkpoint, restore_checkpoint};
pub(crate) use model::Summarizer;
pub(crate) use summarize::SummaryModel;
use trace::Optional;
pub(crate) use trace::Tracer;
pub use trace::{CompactionEvent, CompactionTraceKind};
pub(crate) use window::{Correction, Size};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionError {
    ContextCapacityExceeded,
    NothingToCompact,
    ModelFailed,
    SummaryIncomplete,
    EmptySummary,
    InvalidCheckpoint,
    NotSaved,
    Cancelled,
}

impl CompactionError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::ContextCapacityExceeded => "ContextCapacityExceeded",
            Self::NothingToCompact => "NothingToCompact",
            Self::ModelFailed => "ModelFailed",
            Self::SummaryIncomplete => "SummaryIncomplete",
            Self::EmptySummary => "EmptySummary",
            Self::InvalidCheckpoint => "InvalidCheckpoint",
            Self::NotSaved => "NotSaved",
            Self::Cancelled => "Cancelled",
        }
    }
}

impl fmt::Display for CompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for CompactionError {}

pub(crate) struct Request<'a> {
    pub(crate) turns: &'a [HistoryTurn<'a>],
    pub(crate) active: bool,
    pub(crate) earlier: Option<&'a Payload>,
    pub(crate) size: Size,
    pub(crate) model: &'a str,
    pub(crate) sends_after_conversation: bool,
    pub(crate) follows_checkpoint: bool,
    pub(crate) trace: Tracer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Compacted {
    pub(crate) payload: Payload,
    pub(crate) text: String,
    pub(crate) cut: Cut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Chosen,
    Summarizing,
}

pub(crate) async fn compact(
    request: Request<'_>,
    summary_model: &mut dyn SummaryModel,
    progress: &mut (dyn FnMut(Step) + Send),
    cancel: &CancellationToken,
) -> Result<Option<Compacted>, CompactionError> {
    let trace = request.trace;
    let Some(chosen) = window::choose(
        request.turns,
        request.active,
        request.size,
        request.model,
        trace,
    )?
    else {
        return Ok(None);
    };
    progress(Step::Chosen);
    if cancel.is_cancelled() {
        return Err(CompactionError::Cancelled);
    }
    progress(Step::Summarizing);
    let size = request.size;
    trace.log(
        false,
        format_args!(
            "room after compaction after_tokens={} fixed_tokens={} kept_tokens={} kept_used={} compacted_tokens={}",
            size.after_tokens(),
            Optional(size.fixed_tokens),
            chosen.kept_tokens,
            chosen.kept_used,
            size.compacted_tokens(chosen.kept_used),
        ),
    );
    let turns = turns_from(request.turns, chosen.cut);
    trace.info(
        CompactionTraceKind::ProviderStart,
        format_args!(
            "model={} turns={} earlier={} store=false",
            request.model,
            turns.len(),
            request.follows_checkpoint,
        ),
    );
    let kept = kept_from(request.turns, chosen.cut);
    let mut counted = Counted {
        model: summary_model,
        summaries: 0,
    };
    let summary = summarize::compact(
        summarize::Request {
            earlier: request.earlier,
            turns: &turns,
            last_turn_open: chosen.splits_last_turn(),
            kept: &kept,
            max_prompt_tokens: size.summary_request_tokens(),
            conversation_room: size
                .room_after_conversation()
                .filter(|_| request.sends_after_conversation),
            max_text_tokens: size.compacted_tokens(chosen.kept_used),
            trace,
        },
        &mut counted,
    )
    .await?;
    let compacted = &summary.compacted;
    trace.info(
        CompactionTraceKind::ProviderCompleted,
        format_args!(
            "model={} summaries={} shown_turns={} turns={} tools={} entries={} used={} text_bytes={} fallback=none",
            request.model,
            counted.summaries,
            compacted.turns.len(),
            compacted.turn_count,
            compacted.tool_count,
            compacted.entries.len(),
            compacted.used.len(),
            summary.text.len(),
        ),
    );
    Ok(Some(Compacted {
        payload: summary.compacted,
        text: summary.text,
        cut: chosen.cut,
    }))
}

struct Counted<'m> {
    model: &'m mut dyn SummaryModel,
    summaries: usize,
}

impl SummaryModel for Counted<'_> {
    fn summarize<'a>(
        &'a mut self,
        prompt: summarize::Prompt<'a>,
    ) -> BoxFuture<'a, Result<String, CompactionError>> {
        self.summaries += 1;
        self.model.summarize(prompt)
    }
}

fn turns_from<'a>(history: &[HistoryTurn<'a>], cut: Cut) -> Vec<summarize::Turn<'a>> {
    let mut turns: Vec<summarize::Turn<'a>> = history[..cut.turns.min(history.len())]
        .iter()
        .map(whole_turn)
        .collect();
    if cut.splits_turn()
        && let Some(split) = history.get(cut.turns)
    {
        let steps = cut.tool_steps.min(split.steps.len());
        let mut items = step_items(&split.steps[..steps]);
        note_items(&split.notes[..compacted_notes(split, cut)], &mut items);
        turns.push(summarize::Turn {
            user: split.user,
            items,
        });
    }
    turns
}

fn kept_from<'a>(history: &[HistoryTurn<'a>], cut: Cut) -> Vec<summarize::Turn<'a>> {
    let mut rest = history.get(cut.turns..).unwrap_or_default();
    let mut kept = Vec::new();
    if cut.splits_turn()
        && let Some((split, after)) = rest.split_first()
    {
        let steps = cut.tool_steps.min(split.steps.len());
        let mut items = step_items(&split.steps[steps..]);
        note_items(&split.notes[compacted_notes(split, cut)..], &mut items);
        if !split.reply.is_empty() {
            items.push(summarize::Item::Assistant(split.reply));
        }
        kept.push(summarize::Turn {
            user: split.user,
            items,
        });
        rest = after;
    }
    kept.extend(rest.iter().map(whole_turn));
    kept
}

fn whole_turn<'a>(turn: &HistoryTurn<'a>) -> summarize::Turn<'a> {
    let mut items = step_items(&turn.steps);
    note_items(&turn.notes, &mut items);
    if !turn.reply.is_empty() {
        items.push(summarize::Item::Assistant(turn.reply));
    }
    summarize::Turn {
        user: turn.user,
        items,
    }
}

fn compacted_notes(split: &HistoryTurn<'_>, cut: Cut) -> usize {
    let steps = &split.steps[..cut.tool_steps.min(split.steps.len())];
    let mut remaining = cut
        .steering
        .saturating_sub(steps.iter().map(|step| steering_count(&step.notes)).sum());
    let mut taken = 0;
    for note in &split.notes {
        if remaining == 0 {
            break;
        }
        if matches!(note, Note::User(_)) {
            remaining -= 1;
        }
        taken += 1;
    }
    taken
}

fn steering_count(notes: &[Note<'_>]) -> usize {
    notes
        .iter()
        .filter(|note| matches!(note, Note::User(_)))
        .count()
}

fn note_items<'a>(notes: &[Note<'a>], items: &mut Vec<summarize::Item<'a>>) {
    for note in notes {
        match note {
            Note::Fx(text) => items.push(summarize::Item::Note(text)),
            Note::User(steering) => {
                if !steering.assistant_prefix.is_empty() {
                    items.push(summarize::Item::Assistant(steering.assistant_prefix));
                }
                items.push(summarize::Item::User(steering.text));
            }
        }
    }
}

fn step_items<'a>(steps: &[ToolStep<'a>]) -> Vec<summarize::Item<'a>> {
    let mut items = Vec::new();
    for step in steps {
        note_items(&step.notes, &mut items);
        if !step.assistant.is_empty() {
            items.push(summarize::Item::Assistant(step.assistant));
        }
        items.extend(step.calls.iter().map(|call| {
            summarize::Item::ToolCall(summarize::ToolCall {
                id: call.id.as_str(),
                name: &call.name,
                arguments: &call.arguments,
            })
        }));
        items.extend(step.results.iter().map(|result| {
            summarize::Item::ToolResult(summarize::ToolResult {
                call_id: result.call_id,
                name: result.tool_name,
                output: result.output,
                failed: result.failed,
            })
        }));
        items.extend(
            step.results
                .iter()
                .flat_map(|result| &result.feedback)
                .map(|feedback| summarize::Item::PermissionFeedback(feedback)),
        );
    }
    items
}

pub(crate) fn text_tokens(text: &str) -> usize {
    let mut estimator = StreamingEstimator::default();
    estimator.consume(text);
    usize::try_from(estimator.estimate()).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests;
