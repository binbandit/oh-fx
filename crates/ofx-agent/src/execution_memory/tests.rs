use ofx_contract::{CommandProcessPresentation, ReplaySource, ToolCallId};

use super::*;

fn assistant(content: &str, calls: &[&str]) -> ChatMessage {
    ChatMessage::Assistant {
        content: Some(content.to_owned()),
        tool_calls: calls
            .iter()
            .map(|id| ToolCall::new(*id, "shell", "{}"))
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
        images: Vec::new(),
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
                binding: None,
            },
            parts_json: "[]".to_owned(),
        }),
    }
}

fn contents(history: &[ChatMessage]) -> Vec<String> {
    history
        .iter()
        .map(|message| match message {
            ChatMessage::User { content, .. } | ChatMessage::System { content } => {
                format!("user:{}", steering_text(content).unwrap_or(content))
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
    assert_eq!(turns[1].notes, [Note::Fx("Summarize what you just did.")]);
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
            ..Cut::default()
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
            ..Cut::default()
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
            ..Cut::default()
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
    assert_eq!(
        turns[0].steps[1].notes,
        [Note::Fx("Summarize what you just did.")]
    );
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
            ..Cut::default()
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
            ..Cut::default()
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
            ..Cut::default()
        },
        ChatMessage::user("<first>"),
    );
    retain(
        &mut history,
        &mut starts,
        Cut {
            turns: 1,
            tool_steps: 0,
            ..Cut::default()
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
fn logged_results_carry_the_process_presentation_their_command_returned() {
    let history = vec![
        ChatMessage::user("go"),
        assistant("", &["a", "b"]),
        result("a", ToolResultStatus::Failure),
        result("b", ToolResultStatus::Success),
    ];
    let turn = history_turn(&history, 0, history.len());
    let recorded = [
        RecordedOutput {
            call_id: ToolCallId::new("a"),
            bytes: 11,
            whole_file: false,
            process: Some(CommandProcessPresentation::ExitCode(3)),
        },
        RecordedOutput {
            call_id: ToolCallId::new("b"),
            bytes: 11,
            whole_file: false,
            process: None,
        },
    ];
    let processes: Vec<Option<CommandProcessPresentation>> = logged_steps(&turn.steps, &recorded)
        .iter()
        .flat_map(|step| step.tool_results.clone())
        .map(|result| result.process)
        .collect();
    assert_eq!(
        processes,
        [Some(CommandProcessPresentation::ExitCode(3)), None]
    );
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
    let lengths = |raw: &[RecordedOutput]| -> Vec<(String, usize, usize)> {
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
    let raw = |call_id: &str, bytes| RecordedOutput {
        call_id: ToolCallId::new(call_id),
        bytes,
        whole_file: false,
        process: None,
    };
    let recorded = [
        raw("dropped", 1),
        raw("a", 70_000),
        raw("b", 11),
        raw("a", 90_000),
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
        lengths(&[raw("other", 5)]),
        [
            ("a".to_owned(), 11, 11),
            ("b".to_owned(), 11, 11),
            ("a".to_owned(), 11, 11),
        ]
    );
}

fn steering(text: &str) -> ChatMessage {
    ChatMessage::user(steering_message(text))
}

fn steering_entries<'a>(turn: &HistoryTurn<'a>) -> Vec<(&'a str, &'a str, usize)> {
    turn.steering()
        .map(|entry| {
            (
                entry.text,
                entry.assistant_prefix,
                entry.after_tool_step_count,
            )
        })
        .collect()
}

#[test]
fn steering_messages_tell_the_model_to_apply_the_update_and_continue() {
    let message = steering_message("focus on rendering");
    assert!(message.contains("live user update"));
    assert!(message.contains("Continue working"));
    assert_eq!(
        message,
        "<user_steering>\nApply this live user update to the current task. Continue working unless the user asks you to stop, the task is complete, or a genuine blocker prevents progress.\n\nfocus on rendering\n</user_steering>"
    );
    assert_eq!(steering_text(&message), Some("focus on rendering"));
    assert_eq!(steering_text("focus on rendering"), None);
    assert_eq!(steering_text("<user_steering>\n</user_steering>"), None);
}

#[test]
fn consumed_steering_is_read_without_its_wrapper() {
    let history = vec![
        ChatMessage::user("ordinary user context"),
        steering("focus on rendering"),
        assistant("continuing", &[]),
        steering("run the focused test"),
    ];
    let turn = history_turn(&history, 0, history.len());
    assert!(turn.steps.is_empty());
    assert_eq!(turn.reply, "");
    assert_eq!(
        steering_entries(&turn),
        [
            ("focus on rendering", "", 0),
            ("run the focused test", "continuing", 0)
        ]
    );
}

#[test]
fn each_steering_message_records_the_tool_step_it_followed() {
    let history = vec![
        ChatMessage::user("go"),
        assistant("", &["first"]),
        result("first", ToolResultStatus::Success),
        steering("after first"),
        assistant("", &["second"]),
        result("second", ToolResultStatus::Success),
        steering("after second"),
    ];
    let turn = history_turn(&history, 0, history.len());
    assert_eq!(turn.steps.len(), 2);
    assert_eq!(
        steering_entries(&turn),
        [("after first", "", 1), ("after second", "", 2)]
    );

    let (mut kept, mut starts) = (history, vec![0]);
    retain(
        &mut kept,
        &mut starts,
        Cut {
            turns: 0,
            tool_steps: 1,
            steering: 1,
        },
        ChatMessage::user("<checkpoint>"),
    );
    assert_eq!(
        contents(&kept),
        [
            "user:<checkpoint>",
            "user:go",
            "assistant::1",
            "tool:second",
            "user:after second"
        ]
    );
    let remaining = history_turn(&kept, starts[0], kept.len());
    assert_eq!(steering_entries(&remaining), [("after second", "", 1)]);
}

#[test]
fn interrupted_execution_memory_keeps_steering_typed_right_after_a_tool_result() {
    let history = vec![
        ChatMessage::user("work"),
        assistant("", &["call_done"]),
        result("call_done", ToolResultStatus::Success),
        steering("check the tests too"),
        ChatMessage::user("custom hint"),
        assistant("on it", &[]),
    ];
    let turn = history_turn(&history, 0, history.len());
    assert_eq!(turn.steps.len(), 1);
    assert_eq!(steering_entries(&turn), [("check the tests too", "", 1)]);
    assert_eq!(
        turn.notes,
        [
            Note::User(turn.steering().next().unwrap()),
            Note::Fx("custom hint")
        ]
    );
    assert_eq!(turn.reply, "on it");
}

#[test]
fn a_reply_with_replay_ends_a_step_before_steering_and_plain_text_prefixes_it() {
    let history = vec![
        ChatMessage::user("go"),
        replayed(),
        steering("one"),
        assistant("partial", &[]),
        steering("two"),
        assistant("Done.", &[]),
    ];
    let turn = history_turn(&history, 0, history.len());
    assert_eq!(turn.steps.len(), 1);
    assert!(turn.steps[0].replay.is_some());
    assert_eq!(
        steering_entries(&turn),
        [("one", "", 1), ("two", "partial", 1)]
    );
    assert_eq!(turn.reply, "Done.");
}

#[test]
fn a_continuation_turn_reads_its_prompt_without_the_wrapper() {
    let history = vec![steering("run the next check"), assistant("Ran it.", &[])];
    let turn = history_turn(&history, 0, history.len());
    assert_eq!(turn.user, "run the next check");
    assert_eq!(turn.steering().count(), 0);
}

#[test]
fn compacting_a_whole_running_turn_covers_the_steering_after_its_last_step() {
    let history = vec![
        ChatMessage::user("go"),
        assistant("", &["a"]),
        result("a", ToolResultStatus::Success),
        assistant("partial", &[]),
        steering("also this"),
    ];
    let (mut kept, mut starts) = (history.clone(), vec![0]);
    retain(
        &mut kept,
        &mut starts,
        Cut {
            turns: 0,
            tool_steps: 1,
            steering: 1,
        },
        ChatMessage::user("<checkpoint>"),
    );
    assert_eq!(contents(&kept), ["user:<checkpoint>", "user:go"]);
    let (mut kept, mut starts) = (history, vec![0]);
    retain(
        &mut kept,
        &mut starts,
        Cut {
            turns: 0,
            tool_steps: 1,
            steering: 0,
        },
        ChatMessage::user("<checkpoint>"),
    );
    assert_eq!(
        contents(&kept),
        [
            "user:<checkpoint>",
            "user:go",
            "assistant:partial:0",
            "user:also this"
        ]
    );
}

#[test]
fn restored_steering_is_read_as_steering_without_a_wrapper() {
    let history = vec![
        ChatMessage::user("go"),
        assistant("", &["a"]),
        result("a", ToolResultStatus::Success),
        ChatMessage::restored_steering("from an earlier session"),
        assistant("Done.", &[]),
    ];
    let turn = history_turn(&history, 0, history.len());
    assert_eq!(
        steering_entries(&turn),
        [("from an earlier session", "", 1)]
    );
    assert_eq!(turn.reply, "Done.");
}

#[test]
fn a_reply_ending_as_a_standalone_step_leaves_the_turn_an_empty_reply() {
    let history = vec![ChatMessage::user("go"), assistant("candidate", &[])];
    let plain = history_turn(&history, 0, history.len());
    assert!(plain.steps.is_empty());
    assert_eq!(plain.reply, "candidate");
    let ended = ended_turn(&history, 0, history.len(), true);
    assert_eq!(ended.reply, "");
    assert_eq!(ended.reply_replay, None);
    assert_eq!(ended.steps.len(), 1);
    assert_eq!(ended.steps[0].assistant, "candidate");
    assert!(ended.steps[0].calls.is_empty());
    assert_eq!(ended.steps[0].end(), 2);
}
