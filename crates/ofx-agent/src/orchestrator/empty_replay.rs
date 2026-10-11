use std::borrow::Cow;
use std::ops::Range;

use ofx_contract::{ChatMessage, ProviderReplay};

use super::{Agent, Stop, Turn, TurnFailure, history_trace};

pub(super) type ProjectedReplay = (usize, Option<ProviderReplay>);

impl Agent {
    pub(super) fn project_history(&self, turn: &mut Turn, instructions: usize) -> Result<(), Stop> {
        if turn.history_replays.is_some() {
            return Ok(());
        }
        let shape = turn.projection.take();
        history_trace::projection_started(turn.trace, shape.as_ref(), instructions);
        turn.history_replays = Some(self.project_empty_history_replay(0..turn.start)?);
        history_trace::projection_ended(turn.trace, shape, instructions);
        Ok(())
    }

    pub(super) fn project_empty_history_replay(
        &self,
        messages: Range<usize>,
    ) -> Result<Vec<ProjectedReplay>, Stop> {
        let Some(source) = self.provider.replay_source(&self.config.model) else {
            return Ok(Vec::new());
        };
        let mut projected = Vec::new();
        let earlier = self.history.iter().enumerate().take(messages.end);
        for (index, message) in earlier.skip(messages.start) {
            let ChatMessage::Assistant {
                content,
                tool_calls,
                provider_replay: Some(replay),
            } = message
            else {
                continue;
            };
            if content.as_deref().is_some_and(|text| !text.is_empty()) || !replay.matches(&source) {
                continue;
            }
            let selected = self
                .provider
                .project_replay(replay, tool_calls, false, true)
                .map_err(|error| Stop::failed(TurnFailure::Provider(error)))?;
            if selected.as_ref() != Some(replay) {
                history_trace::empty_replay_projected(
                    index - messages.start,
                    replay.parts_json.len(),
                    selected.as_ref().map_or(0, |kept| kept.parts_json.len()),
                );
                projected.push((index, selected));
            }
        }
        Ok(projected)
    }
}

pub(super) fn apply_projected(history: &mut Cow<'_, [ChatMessage]>, projected: &[ProjectedReplay]) {
    for (index, replay) in projected {
        if let Some(ChatMessage::Assistant {
            provider_replay, ..
        }) = history.to_mut().get_mut(*index)
        {
            provider_replay.clone_from(replay);
        }
    }
}
