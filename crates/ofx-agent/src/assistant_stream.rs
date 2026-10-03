use std::mem;

use ofx_text::{Script, sanitize_assistant_text};

use crate::response_language::evidence;

const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];
const LANGUAGE_PROBE_LIMIT_BYTES: usize = 4096;
const FIRST_LANGUAGE_PROBE_BYTES: usize = 5;

#[derive(Debug, Default)]
pub(crate) struct LanguageStage {
    expected: Option<Script>,
    accepted: bool,
    hold_until_completion: bool,
    next_probe_bytes: usize,
    staged: String,
}

impl LanguageStage {
    pub(crate) fn begin_request(&mut self, expected: Option<Script>, hold_until_completion: bool) {
        self.expected = expected;
        self.accepted = false;
        self.hold_until_completion = hold_until_completion;
        self.next_probe_bytes = FIRST_LANGUAGE_PROBE_BYTES;
        self.staged.clear();
    }

    pub(crate) fn admit(&mut self, text: String) -> Option<String> {
        if !self.staging() {
            return Some(text);
        }
        self.staged.push_str(&text);
        if self.hold_until_completion || self.staged.len() < self.next_probe_bytes {
            return None;
        }
        let prefix = &self.staged[..self.staged.floor_char_boundary(LANGUAGE_PROBE_LIMIT_BYTES)];
        if evidence(prefix).script == self.expected {
            return self.accept();
        }
        self.next_probe_bytes = if self.staged.len() >= LANGUAGE_PROBE_LIMIT_BYTES {
            usize::MAX
        } else {
            LANGUAGE_PROBE_LIMIT_BYTES
                .min((self.staged.len() + 1).max(self.staged.len().saturating_mul(2)))
        };
        None
    }

    pub(crate) fn accept(&mut self) -> Option<String> {
        if !self.staging() {
            return None;
        }
        self.accepted = true;
        Some(mem::take(&mut self.staged)).filter(|text| !text.is_empty())
    }

    pub(crate) fn drop_candidate(&mut self) {
        if !self.staging() {
            return;
        }
        self.staged.clear();
        self.next_probe_bytes = FIRST_LANGUAGE_PROBE_BYTES;
    }

    pub(crate) fn interruption_source<'a>(&self, candidate: &'a str) -> &'a str {
        if !self.staging() {
            return candidate;
        }
        match evidence(candidate).script {
            Some(actual) if Some(actual) != self.expected => "",
            _ => candidate,
        }
    }

    pub(crate) fn expected(&self) -> Option<Script> {
        self.expected
    }

    fn staging(&self) -> bool {
        self.expected.is_some() && !self.accepted
    }
}

pub fn normalize_assistant_text_for_display(raw_text: &str) -> String {
    let base = sanitize_assistant_text(raw_text);
    let visible = base.trim_start_matches(TRIMMED);
    let normalized: String = visible
        .chars()
        .filter(|character| !matches!(character, '*' | '`'))
        .collect();
    let trimmed_length = normalized.trim_end_matches(TRIMMED).len();
    let mut normalized = normalized;
    normalized.truncate(trimmed_length);
    normalized
}

pub fn text_for_completed_presentation<'a>(raw_text: &'a str, normalized_text: &'a str) -> &'a str {
    let fenced = raw_text.split('\n').any(|line| {
        let trimmed = line.trim_start_matches([' ', '\t']);
        trimmed.starts_with("```") || trimmed.starts_with("~~~")
    });
    if fenced { raw_text } else { normalized_text }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_presentation_preserves_fenced_code_while_ordinary_text_stays_normalized() {
        let fenced =
            "```\nconst hook = await resumeHook(token, { cleanup: true } as CleanupSignal);\n```";
        let tilde_fenced = "~~~zig\nconst hook = await resumeHook(token, { cleanup: true } as CleanupSignal);\n~~~";
        let normalized =
            "const hook = await resumeHook(token, { cleanup: true } as CleanupSignal);";
        assert_eq!(text_for_completed_presentation(fenced, normalized), fenced);
        assert_eq!(
            text_for_completed_presentation(tilde_fenced, normalized),
            tilde_fenced
        );
        assert_eq!(
            text_for_completed_presentation(" **Hello** ", "Hello"),
            "Hello"
        );
    }

    #[test]
    fn normalization_strips_emphasis_and_code_marks_and_trims_the_edges() {
        assert_eq!(
            normalize_assistant_text_for_display("\n\n  Hello\n\n**bold** `x`  \n\n"),
            "Hello\n\nbold x"
        );
        assert_eq!(normalize_assistant_text_for_display(" \t\r\n"), "");
        assert_eq!(
            normalize_assistant_text_for_display("I'm oh-fx, your assistant.\nAnswer *here*."),
            "Answer here."
        );
        assert_eq!(normalize_assistant_text_for_display("Done."), "Done.");
    }
}
