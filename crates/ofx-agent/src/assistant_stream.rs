use ofx_text::sanitize_assistant_text;

const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];

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
