use super::KeywordCase;

const CAPACITY: usize = 32;
const LONGEST: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Words<Text: ?Sized = [u8]> {
    first_of_len: [u8; LONGEST + 2],
    starts_by_len: [u8; CAPACITY],
    text: Text,
}

impl<const N: usize> Words<[u8; N]> {
    pub(crate) const fn new(text: &[u8; N]) -> Self {
        let mut first_of_len = [0; LONGEST + 2];
        let mut start: u8 = 0;
        while (start as usize) < N {
            let len = word_len(text, start);
            first_of_len[len as usize + 1] += 1;
            start += len + 1;
        }
        let mut len = 1;
        while len < first_of_len.len() {
            first_of_len[len] += first_of_len[len - 1];
            len += 1;
        }
        let mut next_of_len = first_of_len;
        let mut starts_by_len = [0; CAPACITY];
        start = 0;
        while (start as usize) < N {
            let len = word_len(text, start);
            starts_by_len[next_of_len[len as usize] as usize] = start;
            next_of_len[len as usize] += 1;
            start += len + 1;
        }
        Self {
            first_of_len,
            starts_by_len,
            text: *text,
        }
    }
}

impl Words {
    pub(crate) fn contains(&self, word: &str, case: KeywordCase) -> bool {
        let Some(&[first, end]) = self.first_of_len.get(word.len()..word.len() + 2) else {
            return false;
        };
        let word = word.as_bytes();
        self.starts_by_len[usize::from(first)..usize::from(end)]
            .iter()
            .any(|&start| {
                let candidate = &self.text[usize::from(start)..][..word.len()];
                match case {
                    KeywordCase::Sensitive => candidate == word,
                    KeywordCase::AsciiInsensitive => candidate.eq_ignore_ascii_case(word),
                }
            })
    }

    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = &[u8]> {
        self.text
            .split(|&byte| byte == b' ')
            .filter(|word| !word.is_empty())
    }
}

const fn word_len(text: &[u8], start: u8) -> u8 {
    let mut len = 0;
    while (start as usize) + (len as usize) < text.len()
        && text[start as usize + len as usize] != b' '
    {
        len += 1;
    }
    assert!(len > 0, "words are separated by single spaces");
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    static KEYWORDS: &Words = &Words::new(b"return if else fn");
    static NONE: &Words = &Words::new(b"");

    #[test]
    fn finds_whole_words_only() {
        for word in ["return", "if", "else", "fn"] {
            assert!(KEYWORDS.contains(word, KeywordCase::Sensitive), "{word}");
        }
        for word in ["els", "if else", "", "f", "returns", "Return"] {
            assert!(!KEYWORDS.contains(word, KeywordCase::Sensitive), "{word}");
        }
        assert!(KEYWORDS.contains("Return", KeywordCase::AsciiInsensitive));
        assert!(!KEYWORDS.contains("a_word_longer_than_any_bucket", KeywordCase::Sensitive));
        assert!(!NONE.contains("", KeywordCase::Sensitive));
    }

    #[test]
    fn yields_each_word_in_written_order() {
        assert_eq!(
            KEYWORDS.iter().collect::<Vec<_>>(),
            [b"return".as_slice(), b"if", b"else", b"fn"]
        );
        assert_eq!(NONE.iter().count(), 0);
    }
}
