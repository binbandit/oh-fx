use super::words::{CAPACITY, LONGEST};
use super::{BlockComment, Detection, KeywordCase, ProfileFlag, SOURCES, SourceProfile, Words};

const WORD_HEADER: usize = LONGEST + 2 + CAPACITY;
const PROFILE_COUNT: usize = SOURCES.len();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Range {
    start: u16,
    len: u16,
}

const fn small_offset(value: usize) -> u16 {
    assert!(value <= u16::MAX as usize);
    let bytes = value.to_le_bytes();
    u16::from_le_bytes([bytes[0], bytes[1]])
}

impl Range {
    const EMPTY: Self = Self { start: 0, len: 0 };

    fn bytes(self) -> &'static [u8] {
        let start = usize::from(self.start);
        &DATA[start..start + usize::from(self.len)]
    }

    fn text(self) -> &'static str {
        std::str::from_utf8(self.bytes()).expect("registry text is UTF-8")
    }
}

#[derive(Clone, Copy)]
struct Entry {
    label: Range,
    aliases: Range,
    line_comments: [Range; 2],
    block_start: Range,
    block_end: Range,
    quotes: Range,
    operators: Range,
    flags: u8,
    keywords: Range,
    literals: Range,
    keyword_case: KeywordCase,
    detection: Detection,
}

impl Entry {
    const EMPTY: Self = Self {
        label: Range::EMPTY,
        aliases: Range::EMPTY,
        line_comments: [Range::EMPTY; 2],
        block_start: Range::EMPTY,
        block_end: Range::EMPTY,
        quotes: Range::EMPTY,
        operators: Range::EMPTY,
        flags: 0,
        keywords: Range::EMPTY,
        literals: Range::EMPTY,
        keyword_case: KeywordCase::Sensitive,
        detection: Detection::None,
    };
}

const fn storage_capacity() -> usize {
    let mut len = 0;
    let mut index = 0;
    while index < PROFILE_COUNT {
        let source = &SOURCES[index];
        len += source.label.len() + source.quotes.len() + source.operators.len();
        len += source.aliases.text().len()
            + source.keywords.text().len()
            + source.literals.text().len()
            + 3 * WORD_HEADER;
        let mut comment = 0;
        while comment < source.line_comments.len() {
            len += source.line_comments[comment].len();
            comment += 1;
        }
        if let Some(block) = source.block_comment {
            len += block.start.len() + block.end.len();
        }
        index += 1;
    }
    len
}

struct Built {
    data: [u8; storage_capacity()],
    len: usize,
    entries: [Entry; PROFILE_COUNT],
    ranges: [Range; PROFILE_COUNT * 10],
    range_count: usize,
}

impl Built {
    const fn insert(&mut self, bytes: &[u8]) -> Range {
        assert!(bytes.len() <= u16::MAX as usize);
        let mut candidate = 0;
        while candidate < self.range_count {
            let range = self.ranges[candidate];
            if range.len as usize == bytes.len() {
                let start = range.start as usize;
                let mut index = 0;
                while index < bytes.len() && self.data[start + index] == bytes[index] {
                    index += 1;
                }
                if index == bytes.len() {
                    return range;
                }
            }
            candidate += 1;
        }
        let start = self.len;
        let mut index = 0;
        while index < bytes.len() {
            self.data[self.len] = bytes[index];
            self.len += 1;
            index += 1;
        }
        assert!(self.len <= u16::MAX as usize);
        let range = Range {
            start: small_offset(start),
            len: small_offset(bytes.len()),
        };
        self.ranges[self.range_count] = range;
        self.range_count += 1;
        range
    }

    const fn words(&mut self, words: &Words) -> Range {
        let (first, starts) = words.buckets();
        let text = words.text();
        let mut encoded = [0; WORD_HEADER + u8::MAX as usize];
        assert!(text.len() <= u8::MAX as usize);
        let mut index = 0;
        while index < first.len() {
            encoded[index] = first[index];
            index += 1;
        }
        index = 0;
        while index < starts.len() {
            encoded[first.len() + index] = starts[index];
            index += 1;
        }
        index = 0;
        while index < text.len() {
            encoded[WORD_HEADER + index] = text[index];
            index += 1;
        }
        self.insert(encoded.split_at(WORD_HEADER + text.len()).0)
    }

    const fn profile(&mut self, source: &SourceProfile) -> Entry {
        let mut entry = Entry::EMPTY;
        entry.label = self.insert(source.label.as_bytes());
        entry.aliases = self.words(source.aliases);
        assert!(source.line_comments.len() <= entry.line_comments.len());
        let mut index = 0;
        while index < source.line_comments.len() {
            entry.line_comments[index] = self.insert(source.line_comments[index].as_bytes());
            index += 1;
        }
        if let Some(block) = source.block_comment {
            entry.block_start = self.insert(block.start.as_bytes());
            entry.block_end = self.insert(block.end.as_bytes());
        }
        entry.quotes = self.insert(source.quotes);
        entry.operators = self.insert(source.operators);
        index = 0;
        while index < source.flags.len() {
            entry.flags |= 1 << source.flags[index] as u8;
            index += 1;
        }
        entry.keywords = self.words(source.keywords);
        entry.literals = self.words(source.literals);
        entry.keyword_case = source.keyword_case;
        entry.detection = source.detection;
        entry
    }
}

const fn build() -> Built {
    let mut built = Built {
        data: [0; storage_capacity()],
        len: 0,
        entries: [Entry::EMPTY; PROFILE_COUNT],
        ranges: [Range::EMPTY; PROFILE_COUNT * 10],
        range_count: 0,
    };
    let mut index = 0;
    while index < PROFILE_COUNT {
        built.entries[index] = built.profile(&SOURCES[index]);
        index += 1;
    }
    built
}

const BUILT: Built = build();

const fn data() -> [u8; BUILT.len] {
    let mut bytes = [0; BUILT.len];
    let mut index = 0;
    while index < bytes.len() {
        bytes[index] = BUILT.data[index];
        index += 1;
    }
    bytes
}

static DATA: [u8; BUILT.len] = data();
static ENTRIES: [Entry; PROFILE_COUNT] = BUILT.entries;

#[derive(Debug, PartialEq, Eq)]
pub struct Profile {
    index: u8,
}

const fn handles() -> [Profile; PROFILE_COUNT] {
    assert!(PROFILE_COUNT <= u8::MAX as usize);
    let mut profiles = [const { Profile { index: 0 } }; PROFILE_COUNT];
    let mut index = 0;
    while index < PROFILE_COUNT {
        profiles[index].index = index.to_le_bytes()[0];
        index += 1;
    }
    profiles
}

pub(crate) static PROFILES: [Profile; PROFILE_COUNT] = handles();

impl Profile {
    fn entry(&self) -> &'static Entry {
        &ENTRIES[usize::from(self.index)]
    }

    pub fn label(&self) -> &'static str {
        self.entry().label.text()
    }
    pub(crate) fn aliases(&self) -> WordView {
        WordView(self.entry().aliases)
    }
    pub(crate) fn keywords(&self) -> WordView {
        WordView(self.entry().keywords)
    }
    pub(crate) fn literals(&self) -> WordView {
        WordView(self.entry().literals)
    }
    pub(crate) fn keyword_case(&self) -> KeywordCase {
        self.entry().keyword_case
    }
    pub(super) fn detection(&self) -> Detection {
        self.entry().detection
    }
    pub(crate) fn line_comments(&self) -> impl Iterator<Item = &'static str> {
        self.entry()
            .line_comments
            .into_iter()
            .filter(|range| range.len != 0)
            .map(Range::text)
    }
    pub(crate) fn block_comment(&self) -> Option<BlockComment> {
        let entry = self.entry();
        (entry.block_start.len != 0).then(|| BlockComment {
            start: entry.block_start.text(),
            end: entry.block_end.text(),
        })
    }
    pub(crate) fn quotes(&self) -> &'static [u8] {
        self.entry().quotes.bytes()
    }
    pub(crate) fn operators(&self) -> &'static [u8] {
        self.entry().operators.bytes()
    }
    fn has(&self, flag: ProfileFlag) -> bool {
        self.entry().flags & (1 << flag as u8) != 0
    }
    pub(crate) fn dollar_vars(&self) -> bool {
        self.has(ProfileFlag::DollarVars)
    }
    pub(crate) fn dash_flags(&self) -> bool {
        self.has(ProfileFlag::DashFlags)
    }
    pub(crate) fn command_words(&self) -> bool {
        self.has(ProfileFlag::CommandWords)
    }
    pub(crate) fn bare_numbers(&self) -> bool {
        !self.has(ProfileFlag::PlainBareNumbers)
    }
    pub fn diff_lines(&self) -> bool {
        self.has(ProfileFlag::DiffLines)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WordView(Range);

impl WordView {
    pub(crate) fn contains(self, word: &str, case: KeywordCase) -> bool {
        let encoded = self.0.bytes();
        let Some(&[first, end]) = encoded[..LONGEST + 2].get(word.len()..word.len() + 2) else {
            return false;
        };
        let word = word.as_bytes();
        let starts = &encoded[LONGEST + 2..WORD_HEADER];
        let text = &encoded[WORD_HEADER..];
        starts[usize::from(first)..usize::from(end)]
            .iter()
            .any(|&start| {
                let candidate = &text[usize::from(start)..][..word.len()];
                match case {
                    KeywordCase::Sensitive => candidate == word,
                    KeywordCase::AsciiInsensitive => candidate.eq_ignore_ascii_case(word),
                }
            })
    }

    #[cfg(test)]
    pub(crate) fn iter(self) -> impl Iterator<Item = &'static [u8]> {
        self.0.bytes()[WORD_HEADER..]
            .split(|&byte| byte == b' ')
            .filter(|word| !word.is_empty())
    }
}
