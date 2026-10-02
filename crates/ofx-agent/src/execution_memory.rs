use ofx_contract::{ChatMessage, ProviderReplay, ToolCall, ToolResultStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolResult<'a> {
    pub(crate) call_id: &'a str,
    pub(crate) tool_name: &'a str,
    pub(crate) output: &'a str,
    pub(crate) failed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolStep<'a> {
    pub(crate) assistant: &'a str,
    pub(crate) replay: Option<&'a ProviderReplay>,
    pub(crate) calls: &'a [ToolCall],
    pub(crate) results: Vec<ToolResult<'a>>,
    end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryTurn<'a> {
    pub(crate) user: &'a str,
    pub(crate) steps: Vec<ToolStep<'a>>,
    pub(crate) reply: &'a str,
    pub(crate) reply_replay: Option<&'a ProviderReplay>,
    start: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Cut {
    pub(crate) turns: usize,
    pub(crate) tool_steps: usize,
}

pub(crate) fn history_turns<'a>(
    history: &'a [ChatMessage],
    starts: &[usize],
) -> Vec<HistoryTurn<'a>> {
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(history.len());
            history_turn(history, *start, end)
        })
        .collect()
}

fn history_turn(history: &[ChatMessage], start: usize, end: usize) -> HistoryTurn<'_> {
    let user = match &history[start] {
        ChatMessage::User { content } => content.as_str(),
        _ => "",
    };
    let mut steps: Vec<ToolStep<'_>> = Vec::new();
    let mut pending: Option<ToolStep<'_>> = None;
    for (index, message) in history.iter().enumerate().take(end).skip(start + 1) {
        match message {
            ChatMessage::Assistant {
                content,
                tool_calls,
                provider_replay,
            } => {
                steps.extend(pending.take());
                let step = ToolStep {
                    assistant: content.as_deref().unwrap_or_default(),
                    replay: provider_replay.as_ref(),
                    calls: tool_calls,
                    results: Vec::new(),
                    end: index + 1,
                };
                if tool_calls.is_empty() {
                    pending = Some(step);
                } else {
                    steps.push(step);
                }
            }
            ChatMessage::Tool {
                call_id,
                tool_name,
                content,
                status,
            } => {
                steps.extend(pending.take());
                let result = ToolResult {
                    call_id: call_id.as_str(),
                    tool_name,
                    output: content,
                    failed: *status == ToolResultStatus::Failure,
                };
                match steps.last_mut() {
                    Some(step) if !step.calls.is_empty() => {
                        step.results.push(result);
                        step.end = index + 1;
                    }
                    _ => steps.push(ToolStep {
                        assistant: "",
                        replay: None,
                        calls: &[],
                        results: vec![result],
                        end: index + 1,
                    }),
                }
            }
            ChatMessage::User { .. } | ChatMessage::System { .. } => {
                steps.extend(pending.take());
            }
        }
    }
    let (reply, reply_replay) = pending.map_or(("", None), |step| (step.assistant, step.replay));
    HistoryTurn {
        user,
        steps,
        reply,
        reply_replay,
        start,
    }
}

pub(crate) fn retain(
    history: &mut Vec<ChatMessage>,
    starts: &mut Vec<usize>,
    cut: Cut,
    checkpoint: ChatMessage,
) {
    let Some(&start) = starts.get(cut.turns) else {
        history.clear();
        history.push(checkpoint);
        starts.clear();
        return;
    };
    let end = starts.get(cut.turns + 1).copied().unwrap_or(history.len());
    let tail_from = if cut.tool_steps == 0 {
        start
    } else {
        let steps = history_turn(history, start, end).steps;
        if cut.tool_steps < steps.len() {
            steps[cut.tool_steps - 1].end
        } else {
            end
        }
    };
    let tail = history.split_off(tail_from);
    let kept_user = (cut.tool_steps > 0).then(|| history.swap_remove(start));
    history.clear();
    history.push(checkpoint);
    history.extend(kept_user);
    let shift = history.len();
    history.extend(tail);
    let moved = starts.split_off(cut.turns);
    starts.clear();
    starts.extend(moved.into_iter().map(|turn_start| {
        if turn_start < tail_from {
            1
        } else {
            turn_start - tail_from + shift
        }
    }));
}

#[cfg(test)]
mod tests;
