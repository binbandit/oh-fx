use ofx_contract::{BoxFuture, ChatMessage, ToolCall, ToolCallId, ToolResultStatus};

use super::summarize::Prompt;
use super::*;
use crate::execution_memory::history_turns;

#[derive(Default)]
struct Notes {
    prompts: Vec<(String, bool)>,
}

impl SummaryModel for Notes {
    fn summarize<'a>(
        &'a mut self,
        prompt: Prompt<'a>,
    ) -> BoxFuture<'a, Result<String, CompactionError>> {
        self.prompts
            .push((prompt.user.to_owned(), prompt.after_conversation));
        Box::pin(async {
            Ok("Turn 1\nIn between: Read the notes.\nT1: read them\n\nTurn in progress\nIn between: Started the rewrite.\nT2: read the plan".to_owned())
        })
    }
}

fn assistant(content: &str, call: Option<&str>) -> ChatMessage {
    ChatMessage::Assistant {
        content: Some(content.to_owned()),
        tool_calls: call
            .map(|id| ToolCall {
                id: ToolCallId::new(id),
                name: "read_file".to_owned(),
                arguments: format!("{{\"path\":\"{id}.md\"}}"),
            })
            .into_iter()
            .collect(),
        provider_replay: None,
    }
}

fn result(id: &str, output: &str) -> ChatMessage {
    ChatMessage::Tool {
        call_id: ToolCallId::new(id),
        tool_name: "read_file".to_owned(),
        content: output.to_owned(),
        status: ToolResultStatus::Success,
    }
}

fn history() -> (Vec<ChatMessage>, Vec<usize>) {
    let notes = "recorded notes ".repeat(400);
    (
        vec![
            ChatMessage::user("read the notes"),
            assistant("", Some("notes")),
            result("notes", &notes),
            assistant("Read them.", None),
            ChatMessage::user("now rewrite them"),
            assistant("", Some("plan")),
            result("plan", &notes),
        ],
        vec![0, 4],
    )
}

fn size() -> Size {
    Size {
        compact_at_tokens: Some(5_000),
        usable_tokens: Some(10_000),
        request_tokens: Some(6_000),
        ..Size::default()
    }
}

#[tokio::test]
async fn a_running_turn_keeps_its_user_message_and_compacts_its_finished_steps() {
    let (history, starts) = history();
    let turns = history_turns(&history, &starts);
    let mut model = Notes::default();
    let mut summarizing = 0;
    let compacted = compact(
        Request {
            turns: &turns,
            active: true,
            earlier: None,
            size: size(),
            model: "m",
            sends_after_conversation: false,
        },
        &mut model,
        &mut || summarizing += 1,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(summarizing, 1);
    assert_eq!(
        compacted.cut,
        Cut {
            turns: 1,
            tool_steps: 1
        }
    );
    assert_eq!(compacted.payload.turns.len(), 1);
    assert_eq!(compacted.payload.turns[0].user, "read the notes");
    assert_eq!(compacted.payload.turns[0].final_reply, "Read them.");
    assert_eq!(compacted.payload.turns[0].work, "Read the notes.");
    let open = compacted.payload.open.as_ref().unwrap();
    assert_eq!(open.work, "Started the rewrite.");
    assert_eq!(open.tools[0].line, "read_file plan.md (6000 bytes)");
    assert!(
        compacted
            .text
            .contains("Turn in progress, whose first user message follows this:")
    );
    assert!(!compacted.text.contains("now rewrite them"));
    assert_eq!(model.prompts.len(), 1);
    assert!(!model.prompts[0].1);
    assert!(model.prompts[0].0.contains("[Turn in progress]\n[User, this message stays in the conversation after the summary]\nnow rewrite them\n"));
}

#[tokio::test]
async fn the_request_after_the_conversation_is_offered_only_when_the_caller_can_send_it() {
    let (history, starts) = history();
    let turns = history_turns(&history, &starts);
    let mut model = Notes::default();
    compact(
        Request {
            turns: &turns,
            active: true,
            earlier: None,
            size: size(),
            model: "m",
            sends_after_conversation: true,
        },
        &mut model,
        &mut || {},
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(model.prompts[0].1);
}

#[tokio::test]
async fn nothing_is_compacted_while_the_conversation_fits() {
    let (history, starts) = history();
    let turns = history_turns(&history, &starts);
    let mut model = Notes::default();
    let roomy = Size {
        compact_at_tokens: Some(800_000),
        usable_tokens: Some(1_000_000),
        request_tokens: None,
        ..Size::default()
    };
    let request = Request {
        turns: &turns[1..],
        active: true,
        earlier: None,
        size: roomy,
        model: "m",
        sends_after_conversation: false,
    };
    let mut summarizing = false;
    assert_eq!(
        compact(
            request,
            &mut model,
            &mut || summarizing = true,
            &CancellationToken::new()
        )
        .await,
        Ok(None)
    );
    assert!(!summarizing);
    assert!(model.prompts.is_empty());
}

#[tokio::test]
async fn a_cancelled_compaction_sends_nothing() {
    let (history, starts) = history();
    let turns = history_turns(&history, &starts);
    let mut model = Notes::default();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let request = Request {
        turns: &turns,
        active: true,
        earlier: None,
        size: size(),
        model: "m",
        sends_after_conversation: false,
    };
    let mut summarizing = false;
    assert_eq!(
        compact(request, &mut model, &mut || summarizing = true, &cancel).await,
        Err(CompactionError::Cancelled)
    );
    assert!(!summarizing);
    assert!(model.prompts.is_empty());
}

#[test]
fn error_codes_use_upstream_names() {
    assert_eq!(
        CompactionError::ContextCapacityExceeded.to_string(),
        "ContextCapacityExceeded"
    );
    assert_eq!(CompactionError::ModelFailed.code(), "ModelFailed");
    assert_eq!(
        CompactionError::SummaryIncomplete.code(),
        "SummaryIncomplete"
    );
    assert_eq!(CompactionError::EmptySummary.code(), "EmptySummary");
}

#[tokio::test]
async fn messages_oh_fx_added_to_a_turn_reach_the_notes_request_as_notes() {
    let notes = "recorded notes ".repeat(400);
    let history = vec![
        ChatMessage::user("read the notes"),
        assistant("", Some("notes")),
        result("notes", &notes),
        assistant("", Some("plan")),
        result("plan", &notes),
        ChatMessage::user("Summarize what you just did."),
        assistant("Read them.", None),
        ChatMessage::user("now rewrite them"),
    ];
    let starts = [0, 7];
    let turns = history_turns(&history, &starts);
    let mut model = Notes::default();
    let compacted = compact(
        Request {
            turns: &turns,
            active: true,
            earlier: None,
            size: size(),
            model: "m",
            sends_after_conversation: false,
        },
        &mut model,
        &mut || {},
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        compacted.cut,
        Cut {
            turns: 1,
            tool_steps: 0
        }
    );
    assert!(model.prompts[0].0.contains(
        "[From oh-fx, not the user]\nSummarize what you just did.\n\n[Assistant]\nRead them.\n"
    ));
}
