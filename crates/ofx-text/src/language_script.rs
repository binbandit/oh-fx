#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Script {
    Latin,
    Cyrillic,
    Arabic,
    Hebrew,
    Devanagari,
    Thai,
    Greek,
    Hangul,
    Japanese,
    Han,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub script: Option<Script>,
    pub letters: usize,
    pub dominant_letters: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProseProfiles {
    pub all: Profile,
    pub non_latin: Profile,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    Double,
    CurlySingle,
    CurlyDouble,
}

#[derive(Default)]
struct Counts {
    latin: usize,
    cyrillic: usize,
    arabic: usize,
    hebrew: usize,
    devanagari: usize,
    thai: usize,
    greek: usize,
    hangul: usize,
    kana: usize,
    han: usize,
}

pub fn dominant_script(text: &str) -> Option<Script> {
    let mut counts = Counts::default();
    for character in text.chars() {
        counts.observe(u32::from(character));
    }
    counts.profile().script
}

pub fn prose_profiles(text: &str) -> ProseProfiles {
    let mut counts = prose_counts(text);
    let all = counts.profile();
    counts.latin = 0;
    ProseProfiles {
        all,
        non_latin: counts.profile(),
    }
}

fn prose_counts(text: &str) -> Counts {
    let mut counts = Counts::default();
    let mut characters = text.chars().peekable();
    let mut in_code = false;
    let mut quote = None;
    let mut delimiter_depth = 0_usize;
    while let Some(character) = characters.next() {
        if character == '`' {
            while characters.next_if_eq(&'`').is_some() {}
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        if let Some(open) = quote {
            if closes(open, character) {
                quote = None;
            }
            continue;
        }
        if delimiter_depth > 0 {
            if opens_delimiter(character) {
                delimiter_depth = delimiter_depth.saturating_add(1);
            } else if closes_delimiter(character) {
                delimiter_depth -= 1;
            }
            continue;
        }
        quote = match character {
            '"' => Some(Quote::Double),
            '\u{2018}' => Some(Quote::CurlySingle),
            '\u{201C}' => Some(Quote::CurlyDouble),
            _ => None,
        };
        if quote.is_some() {
            continue;
        }
        if opens_delimiter(character) {
            delimiter_depth = 1;
            continue;
        }
        counts.observe(u32::from(character));
    }
    counts
}

fn closes(quote: Quote, character: char) -> bool {
    match quote {
        Quote::Double => character == '"',
        Quote::CurlySingle => character == '\u{2019}',
        Quote::CurlyDouble => character == '\u{201D}',
    }
}

fn opens_delimiter(character: char) -> bool {
    matches!(
        character,
        '(' | '[' | '{' | '\u{FF08}' | '\u{3010}' | '\u{300C}'
    )
}

fn closes_delimiter(character: char) -> bool {
    matches!(
        character,
        ')' | ']' | '}' | '\u{FF09}' | '\u{3011}' | '\u{300D}'
    )
}

impl Counts {
    fn observe(&mut self, codepoint: u32) {
        let count = match codepoint {
            0x41..=0x5A | 0x61..=0x7A | 0xC0..=0x24F | 0x1E00..=0x1EFF => &mut self.latin,
            0x400..=0x52F | 0x2DE0..=0x2DFF | 0xA640..=0xA69F => &mut self.cyrillic,
            0x600..=0x6FF | 0x750..=0x77F | 0x8A0..=0x8FF | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF => {
                &mut self.arabic
            }
            0x590..=0x5FF => &mut self.hebrew,
            0x900..=0x97F | 0xA8E0..=0xA8FF => &mut self.devanagari,
            0xE00..=0xE7F => &mut self.thai,
            0x370..=0x3FF | 0x1F00..=0x1FFF => &mut self.greek,
            0x1100..=0x11FF
            | 0x3130..=0x318F
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7AF
            | 0xD7B0..=0xD7FF => &mut self.hangul,
            0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F => &mut self.kana,
            0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x20000..=0x2A6DF
            | 0x2A700..=0x2B73F
            | 0x2B740..=0x2B81F
            | 0x2B820..=0x2CEAF => &mut self.han,
            _ => return,
        };
        *count += 1;
    }

    fn letters(&self) -> usize {
        self.latin
            + self.cyrillic
            + self.arabic
            + self.hebrew
            + self.devanagari
            + self.thai
            + self.greek
            + self.hangul
            + self.kana
            + self.han
    }

    fn profile(&self) -> Profile {
        let letters = self.letters();
        if self.kana > 0 {
            return Profile {
                script: Some(Script::Japanese),
                letters,
                dominant_letters: self.kana + self.han,
            };
        }
        if self.hangul > 0 {
            return Profile {
                script: Some(Script::Hangul),
                letters,
                dominant_letters: self.hangul,
            };
        }
        let candidates = [
            (Script::Han, self.han),
            (Script::Arabic, self.arabic),
            (Script::Hebrew, self.hebrew),
            (Script::Cyrillic, self.cyrillic),
            (Script::Greek, self.greek),
            (Script::Devanagari, self.devanagari),
            (Script::Thai, self.thai),
            (Script::Latin, self.latin),
        ];
        let mut best: Option<Script> = None;
        let mut best_count = 0;
        let mut tied = false;
        for (script, count) in candidates {
            if count == 0 {
                continue;
            }
            if count > best_count {
                best = Some(script);
                best_count = count;
                tied = false;
            } else if count == best_count {
                tied = true;
            }
        }
        Profile {
            script: if tied { None } else { best },
            letters,
            dominant_letters: best_count,
        }
    }
}

#[cfg(test)]
mod tests;
