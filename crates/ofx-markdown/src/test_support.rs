use std::fmt::Write;

use crate::assistant_presentation::Event;
use crate::styled::{Attr, Hang, Hyperlink, Line, Slot, Span, Style};

const SLOT_CODES: [(Slot, &str); 3] = [
    (Slot::InlineCode, "38;5;245"),
    (Slot::Link, "38;5;75"),
    (Slot::TaskCompleted, "38;5;252"),
];

#[derive(Default)]
struct ExpectedLine {
    spans: Vec<Span>,
    style: Style,
    link: Option<Hyperlink>,
}

impl ExpectedLine {
    fn push(&mut self, character: char) {
        if let Some(last) = self.spans.last_mut()
            && last.style == self.style
            && last.link == self.link
        {
            last.text.push(character);
            return;
        }
        self.spans.push(Span {
            text: character.to_string(),
            style: self.style,
            link: self.link.clone(),
        });
    }

    fn take(&mut self, newline: bool) -> Line {
        Line {
            spans: std::mem::take(&mut self.spans),
            hang: Hang::None,
            newline,
        }
    }

    fn apply_sgr(&mut self, params: &str) {
        if let Some(&(slot, _)) = SLOT_CODES.iter().find(|(_, code)| *code == params) {
            self.style.slot = Some(slot);
            return;
        }
        if params == "39" {
            self.style.slot = None;
            return;
        }
        self.style = match params {
            "1" => self.style.with(Attr::Bold),
            "2" => self.style.with(Attr::Dim),
            "3" => self.style.with(Attr::Italic),
            "4" => self.style.with(Attr::Underline),
            "9" => self.style.with(Attr::Strike),
            "22" => self.style.without(Attr::Bold).without(Attr::Dim),
            "23" => self.style.without(Attr::Italic),
            "24" => self.style.without(Attr::Underline),
            "29" => self.style.without(Attr::Strike),
            "0" | "" => Style::default(),
            other => panic!("unsupported SGR parameters {other:?}"),
        };
    }
}

pub(crate) fn ansi_lines(ansi: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut line = ExpectedLine::default();
    let mut rest = ansi;
    while let Some(character) = rest.chars().next() {
        if let Some(sequence) = rest.strip_prefix("\x1b[") {
            let end = sequence
                .find('m')
                .expect("expected ANSI uses terminated SGR sequences");
            line.apply_sgr(&sequence[..end]);
            rest = &sequence[end + 1..];
            continue;
        }
        if let Some(sequence) = rest.strip_prefix("\x1b]8;") {
            let end = sequence
                .find("\x1b\\")
                .expect("expected ANSI uses terminated OSC 8 sequences");
            let (params, url) = sequence[..end]
                .split_once(';')
                .expect("OSC 8 carries params and a URL");
            line.link = (!url.is_empty()).then(|| Hyperlink {
                id: params
                    .strip_prefix("id=fx-")
                    .and_then(|id| id.parse().ok())
                    .unwrap_or(0),
                url: url.to_owned(),
            });
            rest = &sequence[end + 2..];
            continue;
        }
        if character == '\n' {
            lines.push(line.take(true));
        } else {
            line.push(character);
        }
        rest = &rest[character.len_utf8()..];
    }
    if !line.spans.is_empty() {
        lines.push(line.take(false));
    }
    lines
}

pub(crate) fn text_lines(events: &[Event]) -> Vec<Line> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Line(line) => Some(line.clone()),
            _ => None,
        })
        .collect()
}

fn normalized(lines: &[Line]) -> Vec<(Vec<Span>, bool)> {
    let mut ids = Vec::new();
    lines
        .iter()
        .map(|line| {
            let spans = line
                .spans
                .iter()
                .map(|span| Span {
                    link: span.link.as_ref().map(|link| Hyperlink {
                        id: normalized_id(&mut ids, link.id),
                        url: link.url.clone(),
                    }),
                    ..span.clone()
                })
                .collect();
            (spans, line.newline)
        })
        .collect()
}

fn normalized_id(ids: &mut Vec<u32>, id: u32) -> u32 {
    let position = ids
        .iter()
        .position(|&known| known == id)
        .unwrap_or_else(|| {
            ids.push(id);
            ids.len() - 1
        });
    u32::try_from(position).expect("few links per test")
}

#[track_caller]
pub(crate) fn assert_lines(lines: &[Line], expected: &str) {
    let actual = normalized(lines);
    let wanted = normalized(&ansi_lines(expected));
    assert_eq!(
        actual,
        wanted,
        "\nactual:   {:?}\nexpected: {:?}",
        canonical_ansi(lines, true),
        canonical_ansi(&ansi_lines(expected), true)
    );
}

#[track_caller]
pub(crate) fn assert_ansi(events: &[Event], expected: &str) {
    assert_lines(&text_lines(events), expected);
}

pub(crate) fn rendered(events: &[Event]) -> String {
    canonical_ansi(&text_lines(events), true)
}

pub(crate) fn canonical(fragment: &str) -> String {
    canonical_ansi(&ansi_lines(fragment), false)
}

pub(crate) fn canonical_closed(fragment: &str) -> String {
    canonical_ansi(&ansi_lines(fragment), true)
}

pub(crate) fn contains_ansi(events: &[Event], fragment: &str) -> bool {
    rendered(events).contains(&canonical(fragment))
}

pub(crate) fn plain_text(events: &[Event]) -> String {
    text_lines(events)
        .iter()
        .map(|line| {
            let mut text = line.text();
            if line.newline {
                text.push('\n');
            }
            text
        })
        .collect()
}

pub(crate) fn has_link(events: &[Event]) -> bool {
    text_lines(events)
        .iter()
        .flat_map(|line| &line.spans)
        .any(|span| span.link.is_some())
}

pub(crate) fn link_urls(events: &[Event]) -> Vec<(u32, String)> {
    let mut links: Vec<(u32, String)> = Vec::new();
    for span in text_lines(events)
        .iter()
        .flat_map(|line| line.spans.clone())
    {
        if let Some(link) = span.link
            && links.last() != Some(&(link.id, link.url.clone()))
        {
            links.push((link.id, link.url));
        }
    }
    links
}

pub(crate) fn hangs(events: &[Event]) -> Vec<Hang> {
    text_lines(events).iter().map(|line| line.hang).collect()
}

fn canonical_ansi(lines: &[Line], close_at_end: bool) -> String {
    let mut out = String::new();
    let mut ids = Vec::new();
    for line in lines {
        let mut style = Style::default();
        let mut link: Option<Hyperlink> = None;
        for span in &line.spans {
            let target_link = span.link.as_ref().map(|link| Hyperlink {
                id: normalized_id(&mut ids, link.id),
                url: link.url.clone(),
            });
            transition(&mut out, &mut style, &mut link, span.style, target_link);
            out.push_str(&span.text);
        }
        if close_at_end || line.newline {
            transition(&mut out, &mut style, &mut link, Style::default(), None);
        }
        if line.newline {
            out.push('\n');
        }
    }
    out
}

fn transition(
    out: &mut String,
    style: &mut Style,
    link: &mut Option<Hyperlink>,
    target: Style,
    target_link: Option<Hyperlink>,
) {
    for (attr, close) in [
        (Attr::Underline, "24"),
        (Attr::Italic, "23"),
        (Attr::Strike, "29"),
    ] {
        if style.has(attr) && !target.has(attr) {
            let _ = write!(out, "\x1b[{close}m");
            *style = style.without(attr);
        }
    }
    let intensity_drops = [Attr::Bold, Attr::Dim]
        .iter()
        .any(|&attr| style.has(attr) && !target.has(attr));
    if intensity_drops {
        out.push_str("\x1b[22m");
        *style = style.without(Attr::Bold).without(Attr::Dim);
    }
    if style.slot.is_some() && style.slot != target.slot {
        out.push_str("\x1b[39m");
        style.slot = None;
    }
    if *link != target_link {
        if link.is_some() {
            out.push_str("\x1b]8;;\x1b\\");
        }
        if let Some(target_link) = &target_link {
            let _ = write!(
                out,
                "\x1b]8;id={};{}\x1b\\",
                target_link.id, target_link.url
            );
        }
        *link = target_link;
    }
    if let Some(slot) = target.slot
        && style.slot != Some(slot)
    {
        let code = SLOT_CODES
            .iter()
            .find(|(known, _)| *known == slot)
            .map_or_else(|| format!("slot:{slot:?}"), |(_, code)| (*code).to_owned());
        let _ = write!(out, "\x1b[{code}m");
        style.slot = Some(slot);
    }
    for (attr, open) in [
        (Attr::Bold, "1"),
        (Attr::Dim, "2"),
        (Attr::Italic, "3"),
        (Attr::Underline, "4"),
        (Attr::Strike, "9"),
    ] {
        if target.has(attr) && !style.has(attr) {
            let _ = write!(out, "\x1b[{open}m");
            *style = style.with(attr);
        }
    }
}
