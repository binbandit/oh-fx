use ofx_contract::{ToolCall, ToolResultStatus};

use super::*;
use crate::execution_memory::close_interrupted_turn;

fn reply(text: &str) -> ChatMessage {
    ChatMessage::Assistant {
        content: Some(text.to_owned()),
        tool_calls: Vec::new(),
        provider_replay: None,
    }
}

fn read_step() -> [ChatMessage; 2] {
    let call = ToolCall::new("call-1", "read_file", r#"{"path":"a"}"#);
    let call_id = call.id.clone();
    [
        ChatMessage::Assistant {
            content: None,
            tool_calls: vec![call],
            provider_replay: None,
        },
        ChatMessage::Tool {
            call_id,
            tool_name: "read_file".to_owned(),
            content: "text".to_owned(),
            status: ToolResultStatus::Success,
        },
    ]
}

fn closed(mut history: Vec<ChatMessage>, start: usize) -> Vec<ChatMessage> {
    let end = history.len();
    close_interrupted_turn(&mut history, start..end);
    history
}

#[test]
fn an_empty_history_projects_no_turns() {
    let shape = HistoryShape::of(&[], &[], &[]);
    assert_eq!(
        shape,
        HistoryShape {
            turns: 0,
            interrupted: 0,
            partial_closures: 0,
            kinds: "none".to_owned(),
            roles: "none".to_owned(),
            messages: 0,
        }
    );
}

#[test]
fn replied_turns_after_a_compaction_summary_are_assistant_turns() {
    let mut history = vec![ChatMessage::user("summary"), ChatMessage::user("read it")];
    history.extend(read_step());
    history.push(reply("done"));
    history.push(ChatMessage::user("thanks"));
    history.push(reply("welcome"));
    let shape = HistoryShape::of(&history, &[1, 5], &[]);
    assert_eq!(shape.turns, 3);
    assert_eq!(shape.kinds, "compacted_summary,assistant,assistant");
    assert_eq!(
        shape.roles,
        "user,user,assistant,tool,assistant,user,assistant"
    );
    assert_eq!((shape.interrupted, shape.partial_closures), (0, 0));
    assert_eq!(shape.messages, 7);
}

#[test]
fn a_closed_interruption_with_partial_text_counts_as_a_partial_closure() {
    let history = closed(
        vec![
            ChatMessage::user("tell me a story"),
            reply("Once the story started"),
        ],
        0,
    );
    let shape = HistoryShape::of(&history, &[0], &[]);
    assert_eq!(shape.kinds, "interrupted");
    assert_eq!(shape.roles, "user,assistant,user");
    assert_eq!((shape.interrupted, shape.partial_closures), (1, 1));
}

#[test]
fn interruptions_without_text_or_with_tools_are_not_partial_closures() {
    let silent = closed(vec![ChatMessage::user("go")], 0);
    let mut with_tools = vec![ChatMessage::user("read it")];
    with_tools.extend(read_step());
    with_tools.push(reply("half"));
    let with_tools = closed(with_tools, 0);
    let mut history = silent.clone();
    let second = history.len();
    history.extend(with_tools);
    let shape = HistoryShape::of(&history, &[0, second], &[]);
    assert_eq!(shape.kinds, "interrupted,interrupted");
    assert_eq!((shape.interrupted, shape.partial_closures), (2, 0));
    assert_eq!(silent.len(), 3);
}

#[test]
fn an_interruption_still_open_for_steering_is_an_interrupted_turn() {
    let history = vec![
        ChatMessage::user("first"),
        reply("answer"),
        ChatMessage::user("second"),
        reply("partial"),
    ];
    let open = Range { start: 2, end: 4 };
    let shape = HistoryShape::of(&history, &[0, 2], &[open]);
    assert_eq!(shape.kinds, "assistant,interrupted");
    assert_eq!((shape.interrupted, shape.partial_closures), (1, 1));
}

#[test]
fn a_prompt_that_quotes_the_closure_note_is_only_interrupted_when_it_ends_the_turn() {
    let history = vec![ChatMessage::user(INTERRUPTED_TURN_CONTEXT), reply("Noted.")];
    let shape = HistoryShape::of(&history, &[0], &[]);
    assert_eq!(shape.kinds, "assistant");
}
