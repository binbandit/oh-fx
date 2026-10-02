use ofx_markdown::Event;

use crate::render_engine::transcript_blocks::{Entry, render_assistant_event};
use crate::row_text::Row;
use crate::theme::Theme;

#[derive(Debug, Default)]
pub(crate) struct Transcript {
    entries: Vec<Entry>,
    rendered: usize,
    pending: Vec<Row>,
    cols: usize,
}

impl Transcript {
    pub(crate) fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
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
                self.pending
                    .extend(render_assistant_event(event, self.cols, theme));
            }
        }
        if let Some(Entry::Assistant { events: existing }) = self.entries.last_mut() {
            existing.extend(events);
        }
    }

    pub(crate) fn tail_wants_footer_gap(&self) -> bool {
        self.entries.last().is_some_and(Entry::wants_footer_gap)
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.rendered = 0;
        self.pending.clear();
    }

    pub(crate) fn restart(&mut self, cols: usize) {
        self.cols = cols;
        self.rendered = 0;
        self.pending.clear();
    }

    pub(crate) fn take_new_rows(&mut self, theme: &Theme) -> Vec<Row> {
        for index in self.rendered..self.entries.len() {
            if index > 0 && !self.entries[index - 1].keeps_trailing_blank() {
                self.pending.push(Row::new());
            }
            self.pending
                .extend(self.entries[index].render(self.cols, theme));
        }
        self.rendered = self.entries.len();
        std::mem::take(&mut self.pending)
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
