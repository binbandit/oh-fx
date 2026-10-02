pub(crate) struct SortedLines<const LINES: usize, const FIRST_CHARS: usize> {
    text: &'static str,
    spans: [(u16, u16); LINES],
    first_chars: [char; FIRST_CHARS],
}

pub(crate) const fn line_count(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut count = 1;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\n' {
            count += 1;
        }
        index += 1;
    }
    count
}

pub(crate) const fn first_char_count(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut count = 1;
    let mut previous = first_char(bytes, 0);
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\n' {
            let first = first_char(bytes, index + 1);
            if first != previous {
                count += 1;
                previous = first;
            }
        }
        index += 1;
    }
    count
}

impl<const LINES: usize, const FIRST_CHARS: usize> SortedLines<LINES, FIRST_CHARS> {
    pub(crate) const fn new(text: &'static str) -> Self {
        let bytes = text.as_bytes();
        let mut spans = [(0, 0); LINES];
        let mut first_chars = [first_char(bytes, 0); FIRST_CHARS];
        let mut line = 0;
        let mut distinct = 1;
        let mut start: u16 = 0;
        let mut end: u16 = 0;
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'\n' {
                spans[line] = (start, end);
                line += 1;
                start = end + 1;
                let first = first_char(bytes, index + 1);
                if first != first_chars[distinct - 1] {
                    first_chars[distinct] = first;
                    distinct += 1;
                }
            }
            end += 1;
            index += 1;
        }
        spans[line] = (start, end);
        assert!(line + 1 == LINES && distinct == FIRST_CHARS);
        Self {
            text,
            spans,
            first_chars,
        }
    }

    pub(crate) fn contains(&self, key: &str) -> bool {
        let index = self
            .spans
            .partition_point(|&span| self.bytes_of(span) < key.as_bytes());
        self.spans
            .get(index)
            .is_some_and(|&span| self.bytes_of(span) == key.as_bytes())
    }

    pub(crate) fn has_line_starting_with(&self, first: char) -> bool {
        self.first_chars.binary_search(&first).is_ok()
    }

    #[cfg(test)]
    pub(crate) fn lines(&self) -> std::str::Lines<'static> {
        self.text.lines()
    }

    fn bytes_of(&self, (start, end): (u16, u16)) -> &'static [u8] {
        &self.text.as_bytes()[usize::from(start)..usize::from(end)]
    }
}

const fn first_char(bytes: &[u8], at: usize) -> char {
    let lead = bytes[at];
    let (len, mut value) = match lead {
        0x00..=0x7f => (1, lead as u32),
        0xc0..=0xdf => (2, (lead & 0x1f) as u32),
        0xe0..=0xef => (3, (lead & 0x0f) as u32),
        _ => (4, (lead & 0x07) as u32),
    };
    let mut index = 1;
    while index < len {
        value = (value << 6) | (bytes[at + index] & 0x3f) as u32;
        index += 1;
    }
    match char::from_u32(value) {
        Some(first) => first,
        None => panic!("every line starts with a character"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRUIT_LINES: &str = "apple\napricot\nbanana\ncherry\n\u{e9}clair";
    static FRUIT: SortedLines<{ line_count(FRUIT_LINES) }, { first_char_count(FRUIT_LINES) }> =
        SortedLines::new(FRUIT_LINES);

    #[test]
    fn finds_whole_lines_only() {
        for line in ["apple", "apricot", "banana", "cherry", "\u{e9}clair"] {
            assert!(FRUIT.contains(line), "{line}");
        }
        for key in ["app", "banana\ncherry", "", "date", "\u{e9}"] {
            assert!(!FRUIT.contains(key), "{key}");
        }
    }

    #[test]
    fn knows_the_first_character_of_every_line() {
        for first in ['a', 'b', 'c', '\u{e9}'] {
            assert!(FRUIT.has_line_starting_with(first), "{first}");
        }
        for first in ['d', 'p', 'e', '\n'] {
            assert!(!FRUIT.has_line_starting_with(first), "{first}");
        }
    }

    #[test]
    fn yields_every_line_in_order() {
        assert_eq!(
            FRUIT.lines().collect::<Vec<_>>(),
            ["apple", "apricot", "banana", "cherry", "\u{e9}clair"]
        );
    }
}
