use ofx_contract::HistoryEntry;
use ofx_markdown::{Completions, MarkdownProcessor};

use super::tool_group_projection::ToolGroup;
use super::tool_presentation::ToolActivityRow;
use crate::output::activity_status::TokenProgress;
use crate::render_engine::transcript_blocks::Entry;

pub(crate) fn replayed_entries(history: Vec<HistoryEntry>) -> impl Iterator<Item = Entry> {
    let mut entries: Vec<Entry> = Vec::with_capacity(history.len());
    for entry in history {
        if let HistoryEntry::Tool(call) = entry {
            let row = ToolActivityRow::saved(call);
            match entries.last_mut() {
                Some(Entry::ToolGroup(group)) => group.push(row),
                _ => entries.push(Entry::ToolGroup(ToolGroup::new(row))),
            }
            continue;
        }
        entries.extend(replayed_entry(entry));
    }
    entries.into_iter()
}

fn replayed_entry(entry: HistoryEntry) -> Option<Entry> {
    match entry {
        HistoryEntry::User(text) => Some(Entry::UserTurn { text }),
        HistoryEntry::Assistant(text) => {
            let mut markdown = MarkdownProcessor::with_completions(Completions::ALL);
            let mut events = Vec::new();
            markdown.push(&text, &mut events);
            markdown.flush(&mut events);
            (!events.is_empty()).then_some(Entry::Assistant { events })
        }
        HistoryEntry::QuestionsAnswered(answers) => Some(Entry::QuestionResolution { answers }),
        HistoryEntry::Cancelled => Some(Entry::Cancellation),
        HistoryEntry::Notice(notice) => Some(Entry::Notice(notice)),
        HistoryEntry::TurnSummary(summary) => Some(Entry::TurnSummary {
            duration_ms: summary.turn_duration_ms,
            progress: TokenProgress {
                input_tokens: summary.token_progress.input_tokens,
                output_tokens: summary.token_progress.output_tokens,
            },
        }),
        HistoryEntry::Tool(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use ofx_contract::{
        HistoryEntry, SavedToolCall, ToolCallId, ToolResultStatus, TurnSummary, TurnTokenProgress,
    };

    use super::replayed_entries;
    use crate::output::activity_status::TokenProgress;
    use crate::render_engine::transcript_blocks::Entry;

    fn tool(id: &str) -> HistoryEntry {
        HistoryEntry::Tool(SavedToolCall {
            call_id: ToolCallId::new(id),
            tool_name: "read_file".to_owned(),
            arguments: "{}".to_owned(),
            description: None,
            status: ToolResultStatus::Success,
            output: String::new(),
            process: None,
            file_change: None,
        })
    }

    #[test]
    fn a_saved_turn_summary_replays_as_the_summary_row_the_live_turn_showed() {
        let summary = TurnSummary {
            started_at_ms: 1000,
            completed_at_ms: 4500,
            thinking_duration_ms: 1200,
            turn_duration_ms: 3500,
            token_progress: TurnTokenProgress {
                input_tokens: 1234,
                output_tokens: 340,
                input_exact: true,
                output_exact: false,
            },
        };
        let entries: Vec<Entry> =
            replayed_entries(vec![HistoryEntry::TurnSummary(summary)]).collect();
        assert!(
            matches!(
                entries.as_slice(),
                [Entry::TurnSummary {
                    duration_ms: 3500,
                    progress: TokenProgress {
                        input_tokens: 1234,
                        output_tokens: 340,
                    },
                }]
            ),
            "{entries:?}"
        );
    }

    #[test]
    fn consecutive_saved_calls_share_a_group_until_another_entry_comes_between() {
        let mut entries: Vec<Entry> = replayed_entries(vec![
            HistoryEntry::User("go".to_owned()),
            tool("c1"),
            tool("c2"),
            HistoryEntry::Assistant("Done.".to_owned()),
            tool("c3"),
        ])
        .collect();
        assert_eq!(entries.len(), 4);
        assert!(matches!(&entries[0], Entry::UserTurn { text } if text == "go"));
        assert!(matches!(&entries[2], Entry::Assistant { .. }));
        let Entry::ToolGroup(first) = &mut entries[1] else {
            panic!("the first two calls form a group");
        };
        assert!(first.row_mut(&ToolCallId::new("c1")).is_some());
        assert!(first.row_mut(&ToolCallId::new("c2")).is_some());
        assert!(first.row_mut(&ToolCallId::new("c3")).is_none());
        let Entry::ToolGroup(second) = &mut entries[3] else {
            panic!("the call after the reply starts a new group");
        };
        assert!(second.row_mut(&ToolCallId::new("c3")).is_some());
    }
}
