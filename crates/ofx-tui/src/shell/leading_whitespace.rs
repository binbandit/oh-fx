use std::borrow::Cow;

#[derive(Debug, Default)]
pub(super) struct LeadingWhitespace {
    saw_visible_text: bool,
    line_prefix: String,
}

impl LeadingWhitespace {
    pub(super) fn release<'a>(&mut self, text: &'a str) -> Option<Cow<'a, str>> {
        if self.saw_visible_text {
            return Some(Cow::Borrowed(text));
        }
        let visible = text.trim_start_matches([' ', '\t', '\r', '\n']);
        for whitespace in text[..text.len() - visible.len()].chars() {
            match whitespace {
                '\n' => self.line_prefix.clear(),
                '\r' => {}
                indent => self.line_prefix.push(indent),
            }
        }
        if visible.is_empty() {
            return None;
        }
        self.saw_visible_text = true;
        let prefix = std::mem::take(&mut self.line_prefix);
        Some(if prefix.starts_with('\t') || prefix.starts_with("    ") {
            Cow::Owned(prefix + visible)
        } else {
            Cow::Borrowed(visible)
        })
    }
}
