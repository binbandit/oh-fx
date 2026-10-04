use ofx_contract::{ToolCallId, TurnOutcome};
use ofx_markdown::Event;

use super::tool_group_projection::ToolGroup;
use super::tool_presentation::ToolActivityRow;
use crate::render_engine::transcript_blocks::{
    Entry, is_blank_line, render_assistant_event, trailing_blank_lines,
};
use crate::row_text::Row;
use crate::theme::Theme;

const REPLAY_TAIL_BYTES: usize = 256 * 1024;
const RETAINED_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Default)]
pub(crate) struct Transcript {
    entries: Vec<Entry>,
    rendered: usize,
    pending: Vec<Row>,
    held_blanks: Vec<Row>,
    cols: usize,
    open_group: Option<usize>,
    provisional: Option<Vec<Row>>,
    replaying: bool,
    entry_bytes: Vec<usize>,
    retained_bytes: usize,
}

impl Transcript {
    pub(crate) fn push(&mut self, entry: Entry) {
        if !matches!(entry, Entry::Notice(_)) {
            self.open_group = None;
        }
        self.held_blanks.clear();
        self.entries.push(entry);
        self.provisional = None;
    }

    pub(crate) fn append_assistant(&mut self, mut events: Vec<Event>, theme: &Theme) {
        if !matches!(self.entries.last(), Some(Entry::Assistant { .. })) {
            let leading = events
                .iter()
                .take_while(|event| is_blank_line(event))
                .count();
            events.drain(..leading);
            if events.is_empty() {
                return;
            }
            self.push(Entry::Assistant { events: Vec::new() });
        }
        if events.is_empty() {
            return;
        }
        if self.rendered == self.entries.len() {
            for event in &events {
                let rows = render_assistant_event(event, self.cols, theme);
                self.retain_rows_of_last_entry(&rows);
                if is_blank_line(event) {
                    self.held_blanks.extend(rows);
                } else {
                    self.pending.append(&mut self.held_blanks);
                    self.pending.extend(rows);
                }
            }
        } else {
            self.provisional = None;
        }
        if let Some(Entry::Assistant { events: existing }) = self.entries.last_mut() {
            existing.extend(events);
        }
    }

    pub(crate) fn add_tool_row(&mut self, row: ToolActivityRow) {
        if let Some(existing) = self
            .tool_row_mut(&row.call_id)
            .filter(|existing| existing.is_active())
        {
            *existing = row;
            return;
        }
        if let Some(Entry::ToolGroup(group)) = self.open_group.map(|index| &mut self.entries[index])
        {
            group.push(row);
        } else {
            self.push(Entry::ToolGroup(ToolGroup::new(row)));
            self.open_group = Some(self.entries.len() - 1);
        }
        self.provisional = None;
    }

    pub(crate) fn tool_row_mut(&mut self, call_id: &ToolCallId) -> Option<&mut ToolActivityRow> {
        let start = self.rendered.min(self.entries.len());
        let row = self.entries[start..]
            .iter_mut()
            .rev()
            .find_map(|entry| match entry {
                Entry::ToolGroup(group) => group.row_mut(call_id),
                _ => None,
            })?;
        self.provisional = None;
        Some(row)
    }

    pub(crate) fn cancel_active_tools(&mut self) -> bool {
        self.settle_active_tools(ToolActivityRow::cancel)
    }

    pub(crate) fn abandon_active_tools(&mut self, outcome: TurnOutcome) -> bool {
        self.settle_active_tools(|row| row.abandon(outcome))
    }

    fn settle_active_tools(&mut self, settle: impl Fn(&mut ToolActivityRow)) -> bool {
        let start = self.rendered.min(self.entries.len());
        let mut settled = false;
        for entry in &mut self.entries[start..] {
            if let Entry::ToolGroup(group) = entry {
                settled |= group.settle_active(&settle);
            }
        }
        self.open_group = None;
        self.provisional = None;
        settled
    }

    pub(crate) fn tail_wants_footer_gap(&self) -> bool {
        self.entries.last().is_some_and(Entry::wants_footer_gap)
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.forget_retained();
        self.rendered = 0;
        self.pending.clear();
        self.held_blanks.clear();
        self.open_group = None;
        self.provisional = None;
    }

    pub(crate) fn restart(&mut self, cols: usize) {
        self.cols = cols;
        self.forget_retained();
        self.rendered = 0;
        self.pending.clear();
        self.held_blanks.clear();
        self.provisional = None;
        self.replaying = false;
    }

    pub(crate) fn replay(&mut self, cols: usize) {
        self.restart(cols);
        self.replaying = true;
    }

    pub(crate) fn take_new_rows(&mut self, theme: &Theme) -> Vec<Row> {
        while self.rendered < self.entries.len() && self.is_final(self.rendered) {
            let index = self.rendered;
            self.push_separator(index);
            let rows = self.entries[index].render(self.cols, theme);
            self.entry_bytes.push(0);
            self.retain_rows_of_last_entry(&rows);
            self.pending.extend(rows);
            self.hold_trailing_blanks(index, theme);
            self.rendered += 1;
            self.provisional = None;
        }
        self.enforce_retention();
        let mut rows = std::mem::take(&mut self.pending);
        if std::mem::take(&mut self.replaying) {
            keep_replay_tail(&mut rows);
        }
        rows
    }

    pub(crate) fn provisional_rows(&mut self, theme: &Theme) -> &[Row] {
        if self.provisional.is_none() {
            let mut rows = Vec::new();
            for index in self.rendered..self.entries.len() {
                if index > 0 && !self.entries[index - 1].keeps_trailing_blank() {
                    rows.push(Row::new());
                }
                rows.extend(self.entries[index].render(self.cols, theme));
            }
            self.provisional = Some(rows);
        }
        self.provisional.as_deref().unwrap_or_default()
    }

    fn is_final(&self, index: usize) -> bool {
        match &self.entries[index] {
            Entry::ToolGroup(group) => group.is_settled() && self.open_group != Some(index),
            _ => true,
        }
    }

    fn hold_trailing_blanks(&mut self, index: usize, theme: &Theme) {
        if index + 1 != self.entries.len() {
            return;
        }
        if let Entry::Assistant { events } = &self.entries[index] {
            for event in &events[events.len() - trailing_blank_lines(events)..] {
                self.held_blanks
                    .extend(render_assistant_event(event, self.cols, theme));
            }
        }
    }

    fn retain_rows_of_last_entry(&mut self, rows: &[Row]) {
        let bytes: usize = rows.iter().map(|row| row.text_len() + 1).sum();
        if let Some(last) = self.entry_bytes.last_mut() {
            *last += bytes;
            self.retained_bytes += bytes;
        }
    }

    fn forget_retained(&mut self) {
        self.entry_bytes.clear();
        self.retained_bytes = 0;
    }

    fn enforce_retention(&mut self) {
        if self.retained_bytes <= RETAINED_TEXT_BYTES {
            return;
        }
        let latest_prompt = self.entries[..self.rendered]
            .iter()
            .rposition(|entry| matches!(entry, Entry::UserTurn { .. }))
            .unwrap_or(self.rendered);
        let mut dropped = 0;
        while self.retained_bytes > RETAINED_TEXT_BYTES && dropped < latest_prompt {
            self.retained_bytes -= self.entry_bytes[dropped];
            dropped += 1;
        }
        if dropped == 0 {
            return;
        }
        self.entries.drain(..dropped);
        self.entry_bytes.drain(..dropped);
        self.rendered -= dropped;
        self.open_group = self.open_group.map(|index| index - dropped);
        self.provisional = None;
    }

    fn push_separator(&mut self, index: usize) {
        if index > 0 && !self.entries[index - 1].keeps_trailing_blank() {
            self.pending.push(Row::new());
        }
    }
}

fn keep_replay_tail(rows: &mut Vec<Row>) {
    let mut budget = REPLAY_TAIL_BYTES;
    let kept = rows
        .iter()
        .rev()
        .take_while(|row| match budget.checked_sub(row.encode().len() + 1) {
            Some(rest) => {
                budget = rest;
                true
            }
            None => false,
        })
        .count();
    rows.drain(..rows.len() - kept);
}

#[cfg(test)]
mod tests {
    use ofx_contract::{
        ActionLabel, CallDescription, Concurrency, Notice, NoticeTone, ToolActivity, ToolEffect,
    };
    use ofx_markdown::{Hang, Line, Span, Style};

    use super::*;

    fn theme() -> Theme {
        Theme::builtin(false, false, true)
    }

    fn line(text: &str) -> Event {
        Event::Line(Line {
            spans: vec![Span {
                text: text.to_owned(),
                style: Style::default(),
                link: None,
            }],
            hang: Hang::None,
            newline: true,
        })
    }

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::text).collect()
    }

    #[test]
    fn blocks_are_separated_by_one_blank_row_except_after_the_welcome() {
        let mut transcript = Transcript::default();
        transcript.restart(80);
        transcript.push(Entry::Welcome {
            version: "1.0.0".to_owned(),
        });
        transcript.push(Entry::UserTurn {
            text: "hi".to_owned(),
        });
        assert!(!transcript.tail_wants_footer_gap());
        transcript.append_assistant(vec![line("Hello.")], &theme());
        assert!(transcript.tail_wants_footer_gap());
        assert_eq!(
            texts(&transcript.take_new_rows(&theme())),
            [
                "oh-fx v1.0.0 · Run /help for commands",
                "",
                "┃ hi",
                "",
                "  Hello."
            ]
        );
        assert!(transcript.take_new_rows(&theme()).is_empty());
    }

    #[test]
    fn streamed_assistant_lines_commit_incrementally() {
        let mut transcript = Transcript::default();
        transcript.restart(80);
        transcript.push(Entry::UserTurn {
            text: "go".to_owned(),
        });
        transcript.append_assistant(vec![line("one")], &theme());
        assert_eq!(
            texts(&transcript.take_new_rows(&theme())),
            ["┃ go", "", "  one"]
        );
        transcript.append_assistant(vec![line("two"), line("three")], &theme());
        assert_eq!(
            texts(&transcript.take_new_rows(&theme())),
            ["  two", "  three"]
        );
        transcript.push(Entry::Notice(Notice::new(NoticeTone::Neutral, "", "done")));
        assert_eq!(texts(&transcript.take_new_rows(&theme())), ["", "* done"]);
        assert!(transcript.pending.is_empty());
        transcript.restart(80);
        assert_eq!(
            texts(&transcript.take_new_rows(&theme())),
            ["┃ go", "", "  one", "  two", "  three", "", "* done"]
        );
    }

    #[test]
    fn restarting_replays_every_block_at_the_new_width() {
        let mut transcript = Transcript::default();
        transcript.restart(80);
        transcript.push(Entry::UserTurn {
            text: "alpha beta".to_owned(),
        });
        transcript.take_new_rows(&theme());
        transcript.restart(8);
        assert_eq!(
            texts(&transcript.take_new_rows(&theme())),
            ["┃ alpha", "┃ beta"]
        );
        transcript.clear();
        assert!(transcript.take_new_rows(&theme()).is_empty());
        assert!(!transcript.tail_wants_footer_gap());
    }

    #[test]
    fn a_replay_keeps_only_its_last_256_kib_while_appends_keep_everything() {
        let lines: Vec<String> = (0..3000)
            .map(|index| format!("{index:04} {}", "x".repeat(95)))
            .collect();
        let mut transcript = Transcript::default();
        transcript.restart(120);
        transcript.push(Entry::UserTurn {
            text: "go".to_owned(),
        });
        transcript.append_assistant(lines.iter().map(|text| line(text)).collect(), &theme());
        let appended = texts(&transcript.take_new_rows(&theme()));
        assert_eq!(appended.len(), 3002);
        assert_eq!(appended[2], format!("  {}", lines[0]));
        transcript.replay(120);
        let replayed = texts(&transcript.take_new_rows(&theme()));
        let bytes: usize = replayed.iter().map(|row| row.len() + 1).sum();
        assert!(bytes <= 256 * 1024, "{bytes}");
        assert!(bytes > 256 * 1024 - 104, "{bytes}");
        assert_eq!(replayed.last(), Some(&format!("  {}", lines[2999])));
        assert!(!replayed.contains(&format!("  {}", lines[0])));
        transcript.restart(120);
        assert_eq!(transcript.take_new_rows(&theme()).len(), 3002);
    }
    fn notice(index: usize) -> Entry {
        Entry::Notice(Notice::new(
            NoticeTone::Neutral,
            "",
            format!("notice {index:05} {}", "z".repeat(90)),
        ))
    }

    fn reading(call: &str) -> ToolActivityRow {
        ToolActivityRow::started(
            ToolCallId::new(call),
            "read_file",
            CallDescription {
                title: "Reading notes.md".to_owned(),
                label: Some(ActionLabel {
                    active: "Reading",
                    completed: "Read",
                    target: "notes.md".to_owned(),
                }),
                activity: ToolActivity::Read,
                effect: ToolEffect::ReadOnly,
                concurrency: Concurrency::Parallel,
            },
        )
    }

    #[test]
    fn final_entries_past_1_mib_of_rendered_text_are_dropped_oldest_first() {
        let mut transcript = Transcript::default();
        transcript.restart(120);
        transcript.push(Entry::Welcome {
            version: "1.0.0".to_owned(),
        });
        for index in 0..12_000 {
            transcript.push(notice(index));
            if index % 100 == 0 {
                transcript.take_new_rows(&theme());
            }
        }
        transcript.take_new_rows(&theme());
        assert!(transcript.retained_bytes <= 1024 * 1024);
        assert!(transcript.retained_bytes > 1024 * 1024 - 200);
        assert_eq!(transcript.entries.len(), transcript.rendered);
        assert!(transcript.entries.len() < 10_000);
        transcript.replay(120);
        let replayed = texts(&transcript.take_new_rows(&theme()));
        assert_eq!(
            replayed.last().map(String::as_str),
            Some(format!("* notice 11999 {}", "z".repeat(90)).as_str())
        );
        assert!(!replayed.iter().any(|row| row.contains("v1.0.0")));
    }

    #[test]
    fn the_latest_prompt_what_followed_it_and_running_tools_are_kept() {
        let mut transcript = Transcript::default();
        transcript.restart(120);
        for index in 0..4000 {
            transcript.push(notice(index));
        }
        transcript.push(Entry::UserTurn {
            text: "latest".to_owned(),
        });
        let long: Vec<String> = (0..12_000)
            .map(|index| format!("reply {index:05} {}", "w".repeat(90)))
            .collect();
        transcript.append_assistant(long.iter().map(|text| line(text)).collect(), &theme());
        transcript.take_new_rows(&theme());
        transcript.add_tool_row(reading("call-1"));
        transcript.take_new_rows(&theme());
        assert!(matches!(transcript.entries[0], Entry::UserTurn { .. }));
        assert_eq!(transcript.entries.len(), 3);
        assert!(
            transcript
                .tool_row_mut(&ToolCallId::new("call-1"))
                .is_some()
        );
        assert!(transcript.cancel_active_tools());
        transcript.push(Entry::UserTurn {
            text: "next".to_owned(),
        });
        transcript.take_new_rows(&theme());
        assert!(matches!(transcript.entries[0], Entry::ToolGroup(_)));
    }

    #[test]
    fn a_transcript_under_1_mib_keeps_every_entry() {
        let mut transcript = Transcript::default();
        transcript.restart(120);
        for index in 0..100 {
            transcript.push(notice(index));
        }
        transcript.take_new_rows(&theme());
        assert_eq!(transcript.entries.len(), 100);
        transcript.replay(80);
        transcript.take_new_rows(&theme());
        assert_eq!(transcript.entries.len(), 100);
        transcript.clear();
        assert_eq!(transcript.retained_bytes, 0);
    }
}
