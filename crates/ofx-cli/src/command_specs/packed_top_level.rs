use super::{SourceTopLevelSpec, TopLevelKind, matches_command_token};
use crate::commands::TOP_LEVEL_SOURCES;

const COUNT: usize = TOP_LEVEL_SOURCES.len();

const fn capacities() -> [usize; 3] {
    let mut counts = [0; 3];
    let mut i = 0;
    while i < COUNT {
        let source = &TOP_LEVEL_SOURCES[i];
        counts[0] += source.token.len() + source.usage.len() + source.summary.len();
        counts[1] += source.aliases.len() + source.details.len();
        counts[2] += source.options.len();
        let mut j = 0;
        while j < source.aliases.len() {
            counts[0] += source.aliases[j].len();
            j += 1;
        }
        j = 0;
        while j < source.details.len() {
            counts[0] += source.details[j].len();
            j += 1;
        }
        j = 0;
        while j < source.options.len() {
            counts[0] += source.options[j].flag.len() + source.options[j].description.len();
            j += 1;
        }
        i += 1;
    }
    counts
}

const CAPACITIES: [usize; 3] = capacities();

#[derive(Clone, Copy, Debug)]
struct Range {
    start: u16,
    len: u16,
}

impl Range {
    const EMPTY: Self = Self { start: 0, len: 0 };

    fn text(self) -> &'static str {
        let start = usize::from(self.start);
        &TEXT[start..start + usize::from(self.len)]
    }

    fn texts(self) -> impl Iterator<Item = &'static str> {
        let start = usize::from(self.start);
        TEXTS[start..start + usize::from(self.len)]
            .iter()
            .map(|range| range.text())
    }
}

const fn small(value: usize) -> u16 {
    assert!(value <= u16::MAX as usize);
    let bytes = value.to_le_bytes();
    u16::from_le_bytes([bytes[0], bytes[1]])
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct OptionDoc {
    flag: Range,
    description: Range,
}

impl OptionDoc {
    const EMPTY: Self = Self {
        flag: Range::EMPTY,
        description: Range::EMPTY,
    };

    pub(crate) fn flag(self) -> &'static str {
        self.flag.text()
    }
    pub(crate) fn description(self) -> &'static str {
        self.description.text()
    }
}

#[derive(Clone, Copy)]
struct Entry {
    token: Range,
    usage: Range,
    summary: Range,
    aliases: Range,
    options: Range,
    details: Range,
    hidden: bool,
}

impl Entry {
    const EMPTY: Self = Self {
        token: Range::EMPTY,
        usage: Range::EMPTY,
        summary: Range::EMPTY,
        aliases: Range::EMPTY,
        options: Range::EMPTY,
        details: Range::EMPTY,
        hidden: false,
    };
}

struct Built {
    data: [u8; CAPACITIES[0]],
    used: usize,
    texts: [Range; CAPACITIES[1]],
    text_count: usize,
    options: [OptionDoc; CAPACITIES[2]],
    option_count: usize,
    entries: [Entry; COUNT],
}

impl Built {
    const fn text(&mut self, text: &str) -> Range {
        let start = self.used;
        let mut i = 0;
        while i < text.len() {
            self.data[self.used] = text.as_bytes()[i];
            self.used += 1;
            i += 1;
        }
        Range {
            start: small(start),
            len: small(text.len()),
        }
    }

    const fn texts(&mut self, texts: &[&str]) -> Range {
        let start = self.text_count;
        let mut i = 0;
        while i < texts.len() {
            self.texts[self.text_count] = self.text(texts[i]);
            self.text_count += 1;
            i += 1;
        }
        Range {
            start: small(start),
            len: small(texts.len()),
        }
    }

    const fn entry(&mut self, source: &SourceTopLevelSpec) -> Entry {
        let token = self.text(source.token);
        let usage = self.text(source.usage);
        let summary = self.text(source.summary);
        let aliases = self.texts(source.aliases);
        let details = self.texts(source.details);
        let start = self.option_count;
        let mut i = 0;
        while i < source.options.len() {
            let flag = self.text(source.options[i].flag);
            let description = self.text(source.options[i].description);
            self.options[self.option_count] = OptionDoc { flag, description };
            self.option_count += 1;
            i += 1;
        }
        Entry {
            token,
            usage,
            summary,
            aliases,
            details,
            options: Range {
                start: small(start),
                len: small(source.options.len()),
            },
            hidden: source.hidden_from_top_level_help,
        }
    }
}

const fn build() -> Built {
    let mut built = Built {
        data: [0; CAPACITIES[0]],
        used: 0,
        texts: [Range::EMPTY; CAPACITIES[1]],
        text_count: 0,
        options: [OptionDoc::EMPTY; CAPACITIES[2]],
        option_count: 0,
        entries: [Entry::EMPTY; COUNT],
    };
    let mut i = 0;
    while i < COUNT {
        built.entries[i] = built.entry(&TOP_LEVEL_SOURCES[i]);
        i += 1;
    }
    built
}

const BUILT: Built = build();
static DATA: [u8; CAPACITIES[0]] = BUILT.data;
const TEXT: &str = match std::str::from_utf8(&DATA) {
    Ok(text) => text,
    Err(_) => panic!("command table text is UTF-8"),
};
static TEXTS: [Range; CAPACITIES[1]] = BUILT.texts;
static OPTIONS: [OptionDoc; CAPACITIES[2]] = BUILT.options;
static ENTRIES: [Entry; COUNT] = BUILT.entries;

#[derive(Debug)]
pub(crate) struct TopLevelSpec {
    pub(crate) kind: TopLevelKind,
}

const fn handles() -> [TopLevelSpec; COUNT] {
    let mut handles = [const {
        TopLevelSpec {
            kind: TopLevelKind::Help,
        }
    }; COUNT];
    let mut i = 0;
    while i < COUNT {
        handles[i].kind = TOP_LEVEL_SOURCES[i].kind;
        i += 1;
    }
    handles
}

pub(crate) static TOP_LEVEL_SPECS: [TopLevelSpec; COUNT] = handles();

impl TopLevelSpec {
    fn entry(&self) -> &'static Entry {
        &ENTRIES[self.kind as usize]
    }
    pub(crate) fn token(&self) -> &'static str {
        self.entry().token.text()
    }
    pub(crate) fn usage(&self) -> &'static str {
        self.entry().usage.text()
    }
    pub(crate) fn summary(&self) -> &'static str {
        self.entry().summary.text()
    }
    pub(crate) fn aliases(&self) -> impl Iterator<Item = &'static str> {
        self.entry().aliases.texts()
    }
    pub(crate) fn details(&self) -> impl Iterator<Item = &'static str> {
        self.entry().details.texts()
    }
    pub(crate) fn options(&self) -> &'static [OptionDoc] {
        let range = self.entry().options;
        let start = usize::from(range.start);
        &OPTIONS[start..start + usize::from(range.len)]
    }
    pub(crate) fn hidden_from_top_level_help(&self) -> bool {
        self.entry().hidden
    }
    pub(crate) fn tokens(&self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.token()).chain(self.aliases())
    }
    pub(crate) fn matches(&self, input: &str) -> bool {
        self.tokens()
            .any(|token| matches_command_token(input, token))
    }
}
