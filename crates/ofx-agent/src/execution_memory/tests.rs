use ofx_contract::{ReplaySource, ToolCallId};

use super::*;

fn assistant(content: &str, calls: &[&str]) -> ChatMessage {
    ChatMessage::Assistant {
        content: Some(content.to_owned()),
        tool_calls: calls
            .iter()
            .map(|id| ToolCall {
                id: ToolCallId::new(*id),
                name: "shell".to_owned(),
                arguments: "{}".to_owned(),
            })
            .collect(),
        provider_replay: None,
    }
}

fn result(id: &str, status: ToolResultStatus) -> ChatMessage {
    ChatMessage::Tool {
        call_id: ToolCallId::new(id),
        tool_name: "shell".to_owned(),
        content: format!("output of {id}"),
        status,
    }
}

fn replayed() -> ChatMessage {
    ChatMessage::Assistant {
        content: Some(String::new()),
        tool_calls: Vec::new(),
        provider_replay: Some(ProviderReplay {
            source: ReplaySource {
                provider: "codex".to_owned(),
                model: "m".to_owned(),
            },
            parts_json: "[]".to_owned(),
        }),
    }
}

fn contents(history: &[ChatMessage]) -> Vec<String> {
    history
        .iter()
        .map(|message| match message {
            ChatMessage::User { content } | ChatMessage::System { content } => {
                format!("user:{content}")
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
                ..
            } => format!(
                "assistant:{}:{}",
                content.as_deref().unwrap_or_default(),
                tool_calls.len()
            ),
            ChatMessage::Tool { call_id, .. } => format!("tool:{}", call_id.as_str()),
        })
        .collect()
}

fn conversation() -> (Vec<ChatMessage>, Vec<usize>) {
    let history = vec![
        ChatMessage::user("first"),
        assistant("Checking.", &["a"]),
        result("a", ToolResultStatus::Success),
        assistant("Done.", &[]),
        ChatMessage::user("second"),
        assistant("", &["b", "c"]),
        result("b", ToolResultStatus::Failure),
        result("c", ToolResultStatus::Success),
        replayed(),
        ChatMessage::user("Summarize what you just did."),
        assistant("Summary.", &[]),
        ChatMessage::user("third"),
        assistant("", &["d"]),
        result("d", ToolResultStatus::Success),
    ];
    (history, vec![0, 4, 11])
}

#[test]
fn turns_read_their_steps_and_final_reply_from_the_messages() {
    let (history, starts) = conversation();
    let turns = history_turns(&history, &starts);
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[0].user, "first");
    assert_eq!(turns[0].steps.len(), 1);
    assert_eq!(turns[0].steps[0].assistant, "Checking.");
    assert_eq!(turns[0].steps[0].results[0].call_id, "a");
    assert_eq!(turns[0].reply, "Done.");

    assert_eq!(turns[1].steps.len(), 2);
    assert_eq!(turns[1].steps[0].calls.len(), 2);
    assert_eq!(turns[1].steps[0].results.len(), 2);
    assert!(turns[1].steps[0].results[0].failed);
    assert!(!turns[1].steps[0].results[1].failed);
    assert!(turns[1].steps[1].calls.is_empty());
    assert!(turns[1].steps[1].replay.is_some());
    assert!(turns[1].steps[1].notes.is_empty());
    assert_eq!(turns[1].notes, ["Summarize what you just did."]);
    assert_eq!(turns[1].reply, "Summary.");

    assert_eq!(turns[2].steps.len(), 1);
    assert_eq!(turns[2].reply, "");
}

#[test]
fn a_cut_at_a_turn_keeps_that_turn_and_everything_after_it() {
    let (mut history, mut starts) = conversation();
    retain(
        &mut history,
        &mut starts,
        Cut {
            turns: 1,
            tool_steps: 0,
        },
        ChatMessage::user("<checkpoint>"),
    );
    assert_eq!(
        contents(&history)[..2],
        ["user:<checkpoint>", "user:second"]
    );
    assert_eq!(history.len(), 11);
    assert_eq!(starts, [1, 8]);
    assert_eq!(history_turns(&history, &starts)[1].user, "third");
}

#[test]
fn a_cut_inside_a_turn_keeps_its_user_message_and_its_later_steps() {
    let (mut history, mut starts) = conversation();
    retain(
        &mut history,
        &mut starts,
        Cut {
            turns: 1,
            tool_steps: 1,
        },
        ChatMessage::user("<checkpoint>"),
    );
    assert_eq!(
        contents(&history),
        [
            "user:<checkpoint>",
            "user:second",
            "assistant::0",
            "user:Summarize what you just did.",
            "assistant:Summary.:0",
            "user:third",
            "assistant::1",
            "tool:d",
        ]
    );
    assert_eq!(starts, [1, 5]);
}

#[test]
fn a_cut_covering_every_step_keeps_the_messages_after_the_last_one() {
    let (mut history, mut starts) = conversation();
    retain(
        &mut history,
        &mut starts,
        Cut {
            turns: 1,
            tool_steps: 2,
        },
        ChatMessage::user("<checkpoint>"),
    );
    assert_eq!(
        contents(&history),
        [
            "user:<checkpoint>",
            "user:second",
            "user:Summarize what you just did.",
            "assistant:Summary.:0",
            "user:third",
            "assistant::1",
            "tool:d",
        ]
    );
    assert_eq!(starts, [1, 4]);
}

#[test]
fn notes_between_steps_belong_to_the_step_after_them() {
    let history = vec![
        ChatMessage::user("start"),
        assistant("", &["a"]),
        result("a", ToolResultStatus::Success),
        ChatMessage::user("Summarize what you just did."),
        assistant("", &["b"]),
        result("b", ToolResultStatus::Success),
        assistant("Done.", &[]),
    ];
    let turns = history_turns(&history, &[0]);
    assert!(turns[0].steps[0].notes.is_empty());
    assert_eq!(turns[0].steps[1].notes, ["Summarize what you just did."]);
    assert!(turns[0].notes.is_empty());
    assert_eq!(turns[0].reply, "Done.");
}

#[test]
fn compacting_every_step_of_the_running_turn_keeps_only_its_user_message() {
    let (mut history, mut starts) = conversation();
    retain(
        &mut history,
        &mut starts,
        Cut {
            turns: 2,
            tool_steps: 1,
        },
        ChatMessage::user("<checkpoint>"),
    );
    assert_eq!(contents(&history), ["user:<checkpoint>", "user:third"]);
    assert_eq!(starts, [1]);

    let (mut history, mut starts) = conversation();
    retain(
        &mut history,
        &mut starts,
        Cut {
            turns: 3,
            tool_steps: 0,
        },
        ChatMessage::user("<checkpoint>"),
    );
    assert_eq!(contents(&history), ["user:<checkpoint>"]);
    assert!(starts.is_empty());
}

#[test]
fn a_later_compaction_replaces_the_earlier_checkpoint() {
    let (mut history, mut starts) = conversation();
    retain(
        &mut history,
        &mut starts,
        Cut {
            turns: 1,
            tool_steps: 0,
        },
        ChatMessage::user("<first>"),
    );
    retain(
        &mut history,
        &mut starts,
        Cut {
            turns: 1,
            tool_steps: 0,
        },
        ChatMessage::user("<second>"),
    );
    assert_eq!(
        contents(&history),
        ["user:<second>", "user:third", "assistant::1", "tool:d"]
    );
    assert_eq!(starts, [1]);
}

#[test]
fn logged_results_carry_the_raw_length_their_tool_returned() {
    let history = vec![
        ChatMessage::user("go"),
        assistant("", &["a", "b"]),
        result("a", ToolResultStatus::Success),
        result("b", ToolResultStatus::Failure),
        assistant("", &["a"]),
        result("a", ToolResultStatus::Success),
    ];
    let turn = history_turn(&history, 0, history.len());
    let lengths = |raw: &[(ToolCallId, usize)]| -> Vec<(String, usize, usize)> {
        logged_steps(&turn.steps, raw)
            .iter()
            .flat_map(|step| step.tool_results.clone())
            .map(|result| {
                (
                    result.call_id.to_owned(),
                    result.output.len(),
                    result.output_bytes,
                )
            })
            .collect()
    };
    let recorded = [
        (ToolCallId::new("dropped"), 1),
        (ToolCallId::new("a"), 70_000),
        (ToolCallId::new("b"), 11),
        (ToolCallId::new("a"), 90_000),
    ];
    assert_eq!(
        lengths(&recorded),
        [
            ("a".to_owned(), 11, 70_000),
            ("b".to_owned(), 11, 11),
            ("a".to_owned(), 11, 90_000),
        ]
    );
    assert_eq!(
        lengths(&recorded[2..]),
        [
            ("a".to_owned(), 11, 11),
            ("b".to_owned(), 11, 11),
            ("a".to_owned(), 11, 11),
        ]
    );
    let swapped = [
        recorded[2].clone(),
        recorded[1].clone(),
        recorded[3].clone(),
    ];
    assert_eq!(
        lengths(&swapped),
        [
            ("a".to_owned(), 11, 11),
            ("b".to_owned(), 11, 11),
            ("a".to_owned(), 11, 90_000),
        ]
    );
    assert_eq!(
        lengths(&[(ToolCallId::new("other"), 5)]),
        [
            ("a".to_owned(), 11, 11),
            ("b".to_owned(), 11, 11),
            ("a".to_owned(), 11, 11),
        ]
    );
}
