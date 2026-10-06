use std::mem;
use std::ops::Range;

use ofx_contract::{
    ChatMessage, HistorySteering, HistoryStep, ProviderReplay, RecordedOutput, StepResult,
    ToolCall, ToolCallId, ToolResultStatus,
};

const STEERING_OPEN: &str = "<user_steering>\nApply this live user update to the current task. Continue working unless the user asks you to stop, the task is complete, or a genuine blocker prevents progress.\n\n";
const STEERING_CLOSE: &str = "\n</user_steering>";

const INTERRUPTED_BEFORE_COMPLETION: &str = "The previous response ended before completion.";
const INTERRUPTED_TURN_CONTEXT: &str = "<turn_aborted>\nThe previous turn ended before completion. Any tools or commands may have partially executed. Do not continue this request unless the user explicitly asks to continue.\n</turn_aborted>";

pub(crate) fn close_interrupted_turn(history: &mut Vec<ChatMessage>, range: Range<usize>) -> usize {
    let Some(messages) = history.get_mut(range.clone()) else {
        return 0;
    };
    let mut closing = Vec::with_capacity(2);
    if let Some(ChatMessage::Assistant {
        content,
        tool_calls,
        ..
    }) = messages.last_mut()
        && tool_calls.is_empty()
    {
        let text = content.get_or_insert_with(String::new);
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(INTERRUPTED_BEFORE_COMPLETION);
    } else if !messages
        .iter()
        .any(|message| matches!(message, ChatMessage::Tool { .. }))
    {
        closing.push(ChatMessage::Assistant {
            content: Some(INTERRUPTED_BEFORE_COMPLETION.to_owned()),
            tool_calls: Vec::new(),
            provider_replay: None,
        });
    }
    closing.push(ChatMessage::user(INTERRUPTED_TURN_CONTEXT));
    let added = closing.len();
    history.splice(range.end..range.end, closing);
    added
}

pub(crate) fn steering_message(text: &str) -> String {
    format!("{STEERING_OPEN}{text}{STEERING_CLOSE}")
}

pub(crate) fn steering_text(content: &str) -> Option<&str> {
    content
        .strip_prefix(STEERING_OPEN)?
        .strip_suffix(STEERING_CLOSE)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolResult<'a> {
    pub(crate) call_id: &'a str,
    pub(crate) tool_name: &'a str,
    pub(crate) output: &'a str,
    pub(crate) failed: bool,
    pub(crate) feedback: Vec<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Steering<'a> {
    pub(crate) text: &'a str,
    pub(crate) assistant_prefix: &'a str,
    pub(crate) after_tool_step_count: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Note<'a> {
    Fx(&'a str),
    User(Steering<'a>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolStep<'a> {
    pub(crate) notes: Vec<Note<'a>>,
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
    pub(crate) notes: Vec<Note<'a>>,
    pub(crate) reply: &'a str,
    pub(crate) reply_replay: Option<&'a ProviderReplay>,
    start: usize,
}

impl ToolStep<'_> {
    pub(crate) fn end(&self) -> usize {
        self.end
    }
}

impl<'a> HistoryTurn<'a> {
    pub(crate) fn logged_steering(&self) -> Vec<HistorySteering<'a>> {
        self.steering()
            .map(|steering| HistorySteering {
                text: steering.text,
                assistant_prefix: steering.assistant_prefix,
                after_tool_step_count: steering.after_tool_step_count,
            })
            .collect()
    }

    pub(crate) fn steering(&self) -> impl Iterator<Item = Steering<'a>> + '_ {
        self.steps
            .iter()
            .flat_map(|step| &step.notes)
            .chain(&self.notes)
            .filter_map(|note| match note {
                Note::User(steering) => Some(*steering),
                Note::Fx(_) => None,
            })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Cut {
    pub(crate) turns: usize,
    pub(crate) tool_steps: usize,
    pub(crate) steering: usize,
}

impl Cut {
    pub(crate) fn splits_turn(self) -> bool {
        self.tool_steps > 0 || self.steering > 0
    }
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

pub(crate) fn history_turn(history: &[ChatMessage], start: usize, end: usize) -> HistoryTurn<'_> {
    let user = match &history[start] {
        ChatMessage::User { content, .. } => steering_text(content).unwrap_or(content),
        _ => "",
    };
    let mut steps: Vec<ToolStep<'_>> = Vec::new();
    let mut pending: Option<ToolStep<'_>> = None;
    let mut notes: Vec<Note<'_>> = Vec::new();
    for (index, message) in history.iter().enumerate().take(end).skip(start + 1) {
        match message {
            ChatMessage::Assistant {
                content,
                tool_calls,
                provider_replay,
            } => {
                steps.extend(pending.take());
                let step = ToolStep {
                    notes: mem::take(&mut notes),
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
                    feedback: Vec::new(),
                };
                push_result(&mut steps, &mut notes, result, index + 1);
            }
            ChatMessage::User {
                content,
                feedback_for: Some(call_id),
                ..
            } => attach_feedback(&mut steps, call_id.as_str(), content, index + 1),
            ChatMessage::User {
                content,
                restored_steering,
                feedback_for: None,
            } => {
                let steering = if *restored_steering {
                    Some(content.as_str())
                } else {
                    steering_text(content)
                };
                let note = if let Some(text) = steering {
                    Note::User(steering_after(
                        text,
                        index + 1,
                        pending.take(),
                        &mut steps,
                        &mut notes,
                    ))
                } else {
                    steps.extend(pending.take());
                    Note::Fx(content)
                };
                notes.push(note);
            }
            ChatMessage::System { .. } => {
                steps.extend(pending.take());
            }
        }
    }
    let (reply, reply_replay) = match pending {
        Some(step) => {
            notes.splice(0..0, step.notes);
            (step.assistant, step.replay)
        }
        None => ("", None),
    };
    HistoryTurn {
        user,
        steps,
        notes,
        reply,
        reply_replay,
        start,
    }
}

fn push_result<'a>(
    steps: &mut Vec<ToolStep<'a>>,
    notes: &mut Vec<Note<'a>>,
    result: ToolResult<'a>,
    end: usize,
) {
    match steps.last_mut() {
        Some(step) if !step.calls.is_empty() => {
            step.results.push(result);
            step.end = end;
        }
        _ => steps.push(ToolStep {
            notes: mem::take(notes),
            assistant: "",
            replay: None,
            calls: &[],
            results: vec![result],
            end,
        }),
    }
}

fn attach_feedback<'a>(steps: &mut [ToolStep<'a>], call_id: &str, text: &'a str, end: usize) {
    if let Some(step) = steps.last_mut()
        && let Some(result) = step
            .results
            .iter_mut()
            .find(|result| result.call_id == call_id)
    {
        result.feedback.push(text);
        step.end = end;
    }
}

fn steering_after<'a>(
    text: &'a str,
    end: usize,
    pending: Option<ToolStep<'a>>,
    steps: &mut Vec<ToolStep<'a>>,
    notes: &mut Vec<Note<'a>>,
) -> Steering<'a> {
    let assistant_prefix = match pending {
        Some(step) if step.replay.is_none() => {
            notes.splice(0..0, step.notes);
            step.assistant
        }
        standalone => {
            steps.extend(standalone);
            ""
        }
    };
    Steering {
        text,
        assistant_prefix,
        after_tool_step_count: steps.len(),
        end,
    }
}

pub(crate) fn partial_view(call_id: ToolCallId, bytes: usize) -> RecordedOutput {
    RecordedOutput {
        call_id,
        bytes,
        whole_file: false,
        process: None,
    }
}

pub(crate) fn logged_steps<'a>(
    steps: &[ToolStep<'a>],
    raw_outputs: &[RecordedOutput],
) -> Vec<HistoryStep<'a>> {
    let kept = steps.iter().map(|step| step.results.len()).sum::<usize>();
    let mut recorded = raw_outputs
        .len()
        .checked_sub(kept)
        .map_or(&[][..], |first| &raw_outputs[first..])
        .iter();
    steps
        .iter()
        .map(|step| HistoryStep {
            assistant: step.assistant,
            provider_replay: step.replay,
            tool_calls: step.calls,
            tool_results: step
                .results
                .iter()
                .map(|result| {
                    let raw = recorded
                        .next()
                        .filter(|raw| raw.call_id.as_str() == result.call_id);
                    StepResult {
                        call_id: result.call_id,
                        tool_name: result.tool_name,
                        output: result.output,
                        output_bytes: raw.map_or(result.output.len(), |raw| raw.bytes),
                        process: raw.and_then(|raw| raw.process),
                        status: if result.failed {
                            ToolResultStatus::Failure
                        } else {
                            ToolResultStatus::Success
                        },
                        model_view_covers_full_file: raw.is_some_and(|raw| raw.whole_file),
                        permission_feedback: result.feedback.clone(),
                    }
                })
                .collect(),
        })
        .collect()
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
    let tail_from = if cut.splits_turn() {
        covered_end(&history_turn(history, start, end), cut).unwrap_or(end)
    } else {
        start
    };
    let tail = history.split_off(tail_from);
    let kept_user = cut.splits_turn().then(|| history.swap_remove(start));
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

fn covered_end(turn: &HistoryTurn<'_>, cut: Cut) -> Option<usize> {
    let steps = cut
        .tool_steps
        .min(turn.steps.len())
        .checked_sub(1)
        .and_then(|last| turn.steps.get(last))
        .map(|step| step.end);
    let steering = cut
        .steering
        .checked_sub(1)
        .and_then(|last| turn.steering().nth(last))
        .map(|steering| steering.end);
    steps.max(steering)
}

mod file_evidence;
#[cfg(test)]
mod tests;

pub(crate) use file_evidence::EarlierEvidence;
