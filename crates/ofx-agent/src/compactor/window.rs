use ofx_contract::{AutoCompactPercent, ProviderReplay};

use super::{CompactionError, text_tokens};
use crate::execution_memory::{Cut, HistoryTurn, ToolStep};

const AFTER_PERCENT: usize = 20;
const KEPT_PERCENT: usize = 40;
const MAX_KEPT_TURNS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Correction {
    pub(crate) estimated: usize,
    pub(crate) measured: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Size {
    pub(crate) compact_at_tokens: Option<usize>,
    pub(crate) usable_tokens: Option<usize>,
    pub(crate) request_tokens: Option<usize>,
    pub(crate) fixed_tokens: Option<usize>,
    pub(crate) correction: Option<Correction>,
    pub(crate) overflow: bool,
}

impl Size {
    pub(crate) fn of(
        context_window: Option<u32>,
        output_tokens: Option<u32>,
        percent: AutoCompactPercent,
    ) -> Self {
        let usable_tokens = usable_input_tokens(context_window, output_tokens);
        Self {
            compact_at_tokens: usable_tokens
                .map(|usable| percent_of(usable, usize::from(percent.get()))),
            usable_tokens,
            ..Self::default()
        }
    }

    pub(crate) fn due(self) -> bool {
        matches!(
            (self.compact_at_tokens, self.request_tokens),
            (Some(at), Some(request)) if request >= at
        )
    }

    fn estimate(self, tokens: usize) -> usize {
        let Some(correction) = self.correction else {
            return tokens;
        };
        if correction.estimated == 0 || correction.measured == 0 || tokens == usize::MAX {
            return tokens;
        }
        let scaled = u128::try_from(tokens).unwrap_or(u128::MAX)
            * u128::try_from(correction.estimated).unwrap_or(u128::MAX)
            / u128::try_from(correction.measured).unwrap_or(u128::MAX);
        usize::try_from(scaled).unwrap_or(usize::MAX)
    }

    pub(crate) fn summary_request_tokens(self) -> usize {
        let mut room = self.usable_tokens.unwrap_or(usize::MAX);
        if let (true, Some(rejected)) = (self.overflow, self.request_tokens) {
            room = room.min(rejected / 4 * 3);
        }
        self.estimate(room)
    }

    pub(crate) fn room_after_conversation(self) -> Option<usize> {
        let usable = self.usable_tokens?;
        let conversation = self.request_tokens?;
        if self.overflow || conversation >= usable {
            return None;
        }
        Some(self.estimate(usable - conversation))
    }

    fn after_tokens(self) -> usize {
        let Some(point) = self.compact_at_tokens.or(self.request_tokens) else {
            return usize::MAX;
        };
        let mut after = point / 2;
        if let Some(usable) = self.usable_tokens {
            after = after.min(percent_of(usable, AFTER_PERCENT));
        }
        if let (true, Some(rejected)) = (self.overflow, self.request_tokens) {
            after = after.min(percent_of(rejected, AFTER_PERCENT));
        }
        after
    }

    fn conversation_tokens(self) -> usize {
        let after = self.after_tokens();
        if after == usize::MAX {
            return after;
        }
        let fixed = self.fixed_tokens.unwrap_or(after / 2);
        self.estimate(after.saturating_sub(fixed).max(after / 4))
    }

    pub(crate) fn compacted_tokens(self, kept_used: usize) -> usize {
        let Some(point) = self.compact_at_tokens.or(self.request_tokens) else {
            return usize::MAX;
        };
        let mut most = point / 2;
        if let (true, Some(rejected)) = (self.overflow, self.request_tokens) {
            most = most.min(rejected / 2);
        }
        let fixed = self.fixed_tokens.unwrap_or(most / 4);
        self.estimate(most.saturating_sub(fixed).max(most / 4))
            .saturating_sub(kept_used)
    }

    fn required(self) -> bool {
        match self.request_tokens {
            None => self.overflow,
            Some(request) => self.overflow || request > self.usable_tokens.unwrap_or(usize::MAX),
        }
    }
}

fn percent_of(tokens: usize, percent: usize) -> usize {
    tokens / 100 * percent + tokens % 100 * percent / 100
}

fn usable_input_tokens(context_window: Option<u32>, output_tokens: Option<u32>) -> Option<usize> {
    let window = usize::try_from(context_window?).ok()?;
    let output = output_tokens.map_or(0, |tokens| usize::try_from(tokens).unwrap_or(usize::MAX));
    Some(window.saturating_sub(output))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Recent {
    cut: Cut,
    tokens: usize,
}

fn select_recent_context(
    turns: &[HistoryTurn<'_>],
    target: usize,
    input_capacity: Option<usize>,
    model: &str,
    max_turns: usize,
) -> Recent {
    let mut raw_count = turns.len();
    let mut selected = Recent {
        cut: Cut {
            turns: raw_count,
            tool_steps: 0,
        },
        tokens: 0,
    };
    let mut total: usize = 0;
    let mut selected_any = false;
    let mut turns_used = 0;
    let over_capacity = |cost: usize| input_capacity.is_some_and(|capacity| cost >= capacity);
    for turn in turns.iter().rev() {
        if selected_any && turns_used >= max_turns {
            break;
        }
        raw_count -= 1;
        let mut base = text_tokens(turn.user)
            .saturating_add(text_tokens(turn.reply))
            .saturating_add(replay_tokens(turn.reply_replay, model))
            .saturating_add(8);
        if turn.steps.is_empty() {
            if !selected_any && over_capacity(base) {
                break;
            }
            if selected_any && (raw_count == 0 || total.saturating_add(base) > target) {
                break;
            }
            total = total.saturating_add(base);
            selected = Recent {
                cut: Cut {
                    turns: raw_count,
                    tool_steps: 0,
                },
                tokens: total,
            };
            selected_any = true;
            turns_used += 1;
            continue;
        }
        let mut turn_counted = false;
        for (step_index, step) in turn.steps.iter().enumerate().rev() {
            let cost = base.saturating_add(step_tokens(step, model));
            if !selected_any && (cost > target || over_capacity(cost)) {
                return selected;
            }
            if selected_any
                && ((raw_count == 0 && step_index == 0) || total.saturating_add(cost) > target)
            {
                return selected;
            }
            total = total.saturating_add(cost);
            selected = Recent {
                cut: Cut {
                    turns: raw_count,
                    tool_steps: step_index,
                },
                tokens: total,
            };
            selected_any = true;
            if !turn_counted {
                turns_used += 1;
            }
            turn_counted = true;
            base = 0;
        }
    }
    selected
}

fn replay_tokens(replay: Option<&ProviderReplay>, model: &str) -> usize {
    replay
        .filter(|replay| replay.source.model == model)
        .map_or(0, |replay| text_tokens(&replay.parts_json))
}

fn step_tokens(step: &ToolStep<'_>, model: &str) -> usize {
    let mut total = replay_tokens(step.replay, model)
        .saturating_add(8)
        .saturating_add(text_tokens(step.assistant));
    for call in step.calls {
        total = total
            .saturating_add(text_tokens(call.id.as_str()))
            .saturating_add(text_tokens(&call.name))
            .saturating_add(text_tokens(&call.arguments))
            .saturating_add(8);
    }
    for result in &step.results {
        total = total
            .saturating_add(text_tokens(result.call_id))
            .saturating_add(text_tokens(result.tool_name))
            .saturating_add(text_tokens(result.output))
            .saturating_add(8);
    }
    total
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    pub(crate) cut: Cut,
    pub(crate) kept_used: usize,
}

impl Window {
    pub(crate) fn has_older(self) -> bool {
        self.cut.turns > 0 || self.cut.tool_steps > 0
    }

    pub(crate) fn splits_last_turn(self) -> bool {
        self.cut.tool_steps > 0
    }
}

pub(crate) fn choose(
    turns: &[HistoryTurn<'_>],
    active: bool,
    size: Size,
    model: &str,
) -> Result<Option<Window>, CompactionError> {
    let mut kept = percent_of(size.conversation_tokens(), KEPT_PERCENT);
    loop {
        let window = split(turns, active, kept, size.usable_tokens, model);
        if window.has_older() {
            return Ok(Some(window));
        }
        if !size.required() {
            return Ok(None);
        }
        if kept == 0 {
            return Err(CompactionError::ContextCapacityExceeded);
        }
        kept = 0;
    }
}

fn split(
    turns: &[HistoryTurn<'_>],
    active: bool,
    kept_tokens: usize,
    input_capacity: Option<usize>,
    model: &str,
) -> Window {
    let recent = if kept_tokens == 0 {
        Recent {
            cut: Cut {
                turns: turns.len(),
                tool_steps: 0,
            },
            tokens: 0,
        }
    } else {
        select_recent_context(turns, kept_tokens, input_capacity, model, MAX_KEPT_TURNS)
    };
    let mut cut = recent.cut;
    if let Some(running) = turns.last().filter(|_| active) {
        let active_index = turns.len() - 1;
        if cut.turns > active_index {
            cut = Cut {
                turns: active_index,
                tool_steps: running.steps.len(),
            };
        }
    }
    Window {
        cut,
        kept_used: recent.tokens,
    }
}

#[cfg(test)]
mod tests;
