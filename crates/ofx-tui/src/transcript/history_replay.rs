use ofx_contract::HistoryEntry;
use ofx_markdown::{Completions, MarkdownProcessor};

use crate::render_engine::transcript_blocks::Entry;

pub(crate) fn replayed_entries(history: Vec<HistoryEntry>) -> impl Iterator<Item = Entry> {
    history.into_iter().filter_map(|entry| match entry {
        HistoryEntry::User(text) => Some(Entry::UserTurn { text }),
        HistoryEntry::Assistant(text) => {
            let mut markdown = MarkdownProcessor::with_completions(Completions::ALL);
            let mut events = Vec::new();
            markdown.push(&text, &mut events);
            markdown.flush(&mut events);
            (!events.is_empty()).then_some(Entry::Assistant { events })
        }
        HistoryEntry::Cancelled => Some(Entry::Cancellation),
        HistoryEntry::Notice(notice) => Some(Entry::Notice(notice)),
    })
}
