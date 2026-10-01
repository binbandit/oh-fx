use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU32, Ordering};

use ofx_text::{escape_terminal_controls, visible_width};

use crate::assistant_presentation::Event;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    InlineCode,
    Link,
    TaskCompleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Attr {
    Bold,
    Dim,
    Italic,
    Underline,
    Strike,
}

impl Attr {
    const fn bit(self) -> u8 {
        match self {
            Self::Bold => 1,
            Self::Dim => 1 << 1,
            Self::Italic => 1 << 2,
            Self::Underline => 1 << 3,
            Self::Strike => 1 << 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    attrs: u8,
    pub slot: Option<Slot>,
}

impl Style {
    pub const fn has(self, attr: Attr) -> bool {
        self.attrs & attr.bit() != 0
    }

    #[must_use]
    pub const fn with(self, attr: Attr) -> Self {
        Self {
            attrs: self.attrs | attr.bit(),
            slot: self.slot,
        }
    }

    #[must_use]
    pub(crate) const fn without(self, attr: Attr) -> Self {
        Self {
            attrs: self.attrs & !attr.bit(),
            slot: self.slot,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Hyperlink {
    pub id: u32,
    pub url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
    pub link: Option<Hyperlink>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Hang {
    #[default]
    None,
    Indent(usize),
    Quote {
        indent: usize,
        depth: usize,
    },
}

impl Hang {
    pub const fn width(self) -> usize {
        match self {
            Self::None => 0,
            Self::Indent(width) => width,
            Self::Quote { indent, depth } => indent + 2 * depth,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<Span>,
    pub hang: Hang,
    pub newline: bool,
}

impl Line {
    pub fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

pub(crate) fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|span| visible_width(&span.text)).sum()
}

static NEXT_LINK_ID: AtomicU32 = AtomicU32::new(1);

pub(crate) fn next_link_id() -> u32 {
    NEXT_LINK_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SpanWriter {
    spans: Vec<Span>,
    style: Style,
    link: Option<Hyperlink>,
    hang: Option<Hang>,
}

impl SpanWriter {
    pub(crate) fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let text = escape_terminal_controls(text);
        if let Some(last) = self.spans.last_mut()
            && last.style == self.style
            && last.link == self.link
        {
            last.text.push_str(&text);
            return;
        }
        self.spans.push(Span {
            text: text.into_owned(),
            style: self.style,
            link: self.link.clone(),
        });
    }

    pub(crate) fn append_spans(&mut self, spans: &[Span]) {
        for span in spans {
            if let Some(last) = self.spans.last_mut()
                && last.style == span.style
                && last.link == span.link
            {
                last.text.push_str(&span.text);
            } else if !span.text.is_empty() {
                self.spans.push(span.clone());
            }
        }
    }

    pub(crate) fn char(&mut self, character: char) {
        self.text(character.encode_utf8(&mut [0; 4]));
    }

    pub(crate) fn repeat(&mut self, text: &str, count: usize) {
        for _ in 0..count {
            self.text(text);
        }
    }

    pub(crate) fn open(&mut self, attr: Attr) {
        self.style = self.style.with(attr);
    }

    pub(crate) fn close(&mut self, attr: Attr) {
        self.style = match attr {
            Attr::Bold | Attr::Dim => self.style.without(Attr::Bold).without(Attr::Dim),
            Attr::Italic | Attr::Underline | Attr::Strike => self.style.without(attr),
        };
    }

    pub(crate) fn open_slot(&mut self, slot: Slot) {
        self.style.slot = Some(slot);
    }

    pub(crate) fn close_slot(&mut self) {
        self.style.slot = None;
    }

    pub(crate) fn open_link(&mut self, url: String) {
        self.link = Some(Hyperlink {
            id: next_link_id(),
            url,
        });
    }

    pub(crate) fn close_link(&mut self) {
        self.link = None;
    }

    pub(crate) fn set_hang(&mut self, hang: Hang) {
        self.hang.get_or_insert(hang);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub(crate) fn take_spans(&mut self) -> Vec<Span> {
        self.hang = None;
        std::mem::take(&mut self.spans)
    }

    pub(crate) fn take_line(&mut self, newline: bool) -> Line {
        let hang = self.hang.take();
        let spans = std::mem::take(&mut self.spans);
        Line {
            hang: hang.unwrap_or_else(|| prose_indent(&spans)),
            spans,
            newline,
        }
    }

    pub(crate) fn reopen(&mut self, line: Line) {
        self.hang = Some(line.hang);
        self.spans = line.spans;
    }
}

pub(crate) struct TextOut<'a> {
    events: &'a mut Vec<Event>,
    first_new_event: usize,
    line: SpanWriter,
}

impl<'a> TextOut<'a> {
    pub(crate) fn new(events: &'a mut Vec<Event>) -> Self {
        Self {
            first_new_event: events.len(),
            events,
            line: SpanWriter::default(),
        }
    }

    pub(crate) fn new_events(&self) -> &[Event] {
        &self.events[self.first_new_event..]
    }

    fn last_new_event(&self) -> Option<&Event> {
        self.new_events().last()
    }

    pub(crate) fn newline(&mut self) {
        let line = self.line.take_line(true);
        self.events.push(Event::Line(line));
    }

    pub(crate) fn push_event(&mut self, event: Event) {
        if !self.line.is_empty() {
            let line = self.line.take_line(false);
            self.events.push(Event::Line(line));
        }
        self.events.push(event);
    }

    pub(crate) fn grew(&self) -> bool {
        self.events.len() > self.first_new_event || !self.line.is_empty()
    }

    pub(crate) fn ends_with_newline(&self) -> bool {
        self.line.is_empty()
            && matches!(self.last_new_event(), Some(Event::Line(line)) if line.newline)
    }

    pub(crate) fn pop_newline(&mut self) {
        if !self.ends_with_newline() {
            return;
        }
        if let Some(Event::Line(line)) = self.events.pop()
            && !line.is_empty()
        {
            self.line.reopen(line);
        }
    }

    pub(crate) fn has_text(&self) -> bool {
        !self.line.is_empty() || matches!(self.last_new_event(), Some(Event::Line(_)))
    }

    pub(crate) fn finish(mut self) {
        if !self.line.is_empty() {
            let line = self.line.take_line(false);
            self.events.push(Event::Line(line));
        }
    }
}

impl Deref for TextOut<'_> {
    type Target = SpanWriter;

    fn deref(&self) -> &SpanWriter {
        &self.line
    }
}

impl DerefMut for TextOut<'_> {
    fn deref_mut(&mut self) -> &mut SpanWriter {
        &mut self.line
    }
}

fn prose_indent(spans: &[Span]) -> Hang {
    let mut width = 0;
    for character in spans.iter().flat_map(|span| span.text.chars()) {
        match character {
            ' ' => width += 1,
            '\u{0}'..='\u{1f}' | '\u{7f}' => return Hang::None,
            _ if width > 0 => return Hang::Indent(width),
            _ => return Hang::None,
        }
    }
    Hang::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_bold_or_dim_clears_both_like_sgr_22() {
        let mut writer = SpanWriter::default();
        writer.open(Attr::Bold);
        writer.open(Attr::Dim);
        writer.text("a");
        writer.close(Attr::Dim);
        writer.text("b");
        let line = writer.take_line(true);
        assert!(line.spans[0].style.has(Attr::Bold));
        assert!(line.spans[0].style.has(Attr::Dim));
        assert_eq!(line.spans[1].style, Style::default());
    }

    #[test]
    fn adjacent_text_with_the_same_style_merges_into_one_span() {
        let mut writer = SpanWriter::default();
        writer.text("a");
        writer.text("b");
        writer.open(Attr::Italic);
        writer.text("c");
        let line = writer.take_line(false);
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].text, "ab");
        assert_eq!(line.text(), "abc");
    }

    #[test]
    fn prose_lines_hang_at_their_leading_spaces() {
        let mut writer = SpanWriter::default();
        writer.text("   indented");
        assert_eq!(writer.take_line(true).hang, Hang::Indent(3));
        writer.text("  \ttab");
        assert_eq!(writer.take_line(true).hang, Hang::None);
        writer.text("   ");
        assert_eq!(writer.take_line(true).hang, Hang::None);
        writer.set_hang(Hang::Quote {
            indent: 1,
            depth: 2,
        });
        writer.set_hang(Hang::Indent(9));
        writer.text("  quoted");
        let line = writer.take_line(true);
        assert_eq!(
            line.hang,
            Hang::Quote {
                indent: 1,
                depth: 2
            }
        );
        assert_eq!(line.hang.width(), 5);
    }
}
