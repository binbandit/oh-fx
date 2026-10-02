use ofx_contract::{ToolCallId, TurnOutcome};
use ofx_markdown::Event;

use super::tool_group_projection::ToolGroup;
use super::tool_presentation::ToolActivityRow;
use crate::render_engine::transcript_blocks::{
    Entry, is_blank_line, render_assistant_event, trailing_blank_lines,
};
use crate::row_text::Row;
use crate::theme::Theme;

#[derive(Debug, Default)]
pub(crate) struct Transcript {
    entries: Vec<Entry>,
    rendered: usize,
    pending: Vec<Row>,
    held_blanks: Vec<Row>,
    cols: usize,
    open_group: bool,
    provisional: Option<Vec<Row>>,
}

impl Transcript {
    pub(crate) fn push(&mut self, entry: Entry) {
        self.open_group = false;
        self.held_blanks.clear();
        self.entries.push(entry);
        self.provisional = None;
    }

    pub(crate) fn append_assistant(&mut self, events: Vec<Event>, theme: &Theme) {
        if events.is_empty() {
            return;
        }
        if !matches!(self.entries.last(), Some(Entry::Assistant { .. })) {
            self.push(Entry::Assistant { events: Vec::new() });
        }
        if self.rendered == self.entries.len() {
            for event in &events {
                let rows = render_assistant_event(event, self.cols, theme);
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
        match self.entries.last_mut() {
            Some(Entry::ToolGroup(group)) if self.open_group => group.push(row),
            _ => {
                self.push(Entry::ToolGroup(ToolGroup::new(row)));
                self.open_group = true;
            }
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
        self.settle_active_tools(ToolGroup::cancel_active)
    }

    pub(crate) fn abandon_active_tools(&mut self, outcome: TurnOutcome) -> bool {
        self.settle_active_tools(|group| group.abandon_active(outcome))
    }

    fn settle_active_tools(&mut self, mut settle: impl FnMut(&mut ToolGroup) -> bool) -> bool {
        let start = self.rendered.min(self.entries.len());
        let mut settled = false;
        for entry in &mut self.entries[start..] {
            if let Entry::ToolGroup(group) = entry {
                settled |= settle(group);
            }
        }
        self.open_group = false;
        self.provisional = None;
        settled
    }

    pub(crate) fn tail_wants_footer_gap(&self) -> bool {
        self.entries.last().is_some_and(Entry::wants_footer_gap)
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.rendered = 0;
        self.pending.clear();
        self.held_blanks.clear();
        self.open_group = false;
        self.provisional = None;
    }

    pub(crate) fn restart(&mut self, cols: usize) {
        self.cols = cols;
        self.rendered = 0;
        self.pending.clear();
        self.held_blanks.clear();
        self.provisional = None;
    }

    pub(crate) fn take_new_rows(&mut self, theme: &Theme) -> Vec<Row> {
        while self.rendered < self.entries.len() && self.is_final(self.rendered) {
            let index = self.rendered;
            self.push_separator(index);
            self.pending
                .extend(self.entries[index].render(self.cols, theme));
            self.hold_trailing_blanks(index, theme);
            self.rendered += 1;
            self.provisional = None;
        }
        std::mem::take(&mut self.pending)
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
            Entry::ToolGroup(group) => {
                group.is_settled() && !(self.open_group && index + 1 == self.entries.len())
            }
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

    fn push_separator(&mut self, index: usize) {
        if index > 0 && !self.entries[index - 1].keeps_trailing_blank() {
            self.pending.push(Row::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use ofx_contract::{Notice, NoticeTone};
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
}
