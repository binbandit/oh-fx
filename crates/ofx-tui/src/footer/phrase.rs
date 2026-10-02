use ofx_text::visible_width;

use super::command_text::{approval_text, suffix_terminal_safe_by_width};

const LEADING_ELLIPSIS: &str = "…";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathText {
    text: String,
    basename_len: usize,
}

impl PathText {
    pub(crate) fn from_raw(raw: &[u8]) -> Self {
        let trimmed = raw.len() - raw.iter().rev().take_while(|byte| **byte == b'/').count();
        let basename_start = raw[..trimmed]
            .iter()
            .rposition(|byte| *byte == b'/')
            .map_or(0, |separator| separator + 1);
        Self {
            text: approval_text(raw),
            basename_len: approval_text(&raw[basename_start..]).len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Phrase {
    head: String,
    path: Option<PathText>,
    tail: String,
}

impl Phrase {
    pub(crate) fn plain(text: impl Into<String>) -> Self {
        Self {
            head: text.into(),
            path: None,
            tail: String::new(),
        }
    }

    pub(crate) fn with_path(head: impl Into<String>, path: PathText, tail: &str) -> Self {
        Self {
            head: head.into(),
            path: Some(path),
            tail: tail.to_owned(),
        }
    }

    #[must_use]
    pub(crate) fn after(mut self, prefix: &str) -> Self {
        self.head.insert_str(0, prefix);
        self
    }

    pub(crate) fn fit(&self, width: usize) -> (String, bool) {
        let path = self.path.as_ref().map_or("", |path| path.text.as_str());
        let whole = format!("{}{path}{}", self.head, self.tail);
        if visible_width(&whole) <= width {
            return (whole, true);
        }
        let Some(path) = &self.path else {
            return (whole, false);
        };
        let fixed =
            visible_width(&self.head) + visible_width(LEADING_ELLIPSIS) + visible_width(&self.tail);
        let room = width.saturating_sub(fixed);
        let suffix = suffix_terminal_safe_by_width(&path.text, room);
        let complete = fixed <= width && suffix.len() >= path.basename_len;
        (
            format!("{}{LEADING_ELLIPSIS}{suffix}{}", self.head, self.tail),
            complete,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_encoded_from_raw_bytes_and_shortened_from_the_front() {
        let phrase = Phrase::with_path(
            "allow reads under ",
            PathText::from_raw(b"/home/me/projects/n\x1b\xffotes"),
            " for this session",
        );
        let (whole, complete) = phrase.fit(200);
        assert_eq!(
            whole,
            "allow reads under /home/me/projects/n\\x1b\\xffotes for this session"
        );
        assert!(complete);
        let (short, complete) = phrase.fit(52);
        assert_eq!(
            short,
            "allow reads under …ts/n\\x1b\\xffotes for this session"
        );
        assert!(complete);
        let (cut, complete) = phrase.fit(46);
        assert_eq!(cut, "allow reads under …\\xffotes for this session");
        assert!(!complete);
    }

    #[test]
    fn a_trailing_slash_keeps_the_last_directory_as_the_name_that_must_show() {
        let phrase = Phrase::with_path("", PathText::from_raw(b"/aaaa/bbbb/"), "");
        assert_eq!(phrase.fit(6), ("…bbbb/".to_owned(), true));
        assert_eq!(phrase.fit(5), ("…bbb/".to_owned(), false));
    }

    #[test]
    fn emoji_presentation_sequences_in_a_path_never_push_its_name_out_of_view() {
        let raw = format!("/home/u/{}/secret_key", "\u{2764}\u{fe0f}".repeat(70));
        let phrase = Phrase::with_path("read_file ", PathText::from_raw(raw.as_bytes()), "");
        let (shown, complete) = phrase.fit(78);
        assert!(complete);
        assert!(visible_width(&shown) <= 78, "{shown}");
        assert!(shown.ends_with("/secret_key"), "{shown}");
    }

    #[test]
    fn plain_phrases_are_complete_only_when_they_fit() {
        assert_eq!(Phrase::plain("3. No").fit(5), ("3. No".to_owned(), true));
        assert_eq!(Phrase::plain("3. No").fit(4), ("3. No".to_owned(), false));
        assert_eq!(
            Phrase::plain("No").after("3. ").fit(9),
            ("3. No".to_owned(), true)
        );
    }
}
