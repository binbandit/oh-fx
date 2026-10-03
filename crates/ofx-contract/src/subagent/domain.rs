pub(crate) const MAX_MODEL_BYTES: usize = 256;
pub(crate) const MAX_PROMPT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_AGENT_NAME_BYTES: usize = 64;
const MAX_INSTRUCTIONS_BYTES: usize = 64 * 1024;

pub fn valid_agent_name(name: &str) -> bool {
    let Some((first, rest)) = name.as_bytes().split_first() else {
        return false;
    };
    name.len() <= MAX_AGENT_NAME_BYTES
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && rest.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

pub fn valid_instructions(instructions: &str) -> bool {
    instructions.len() <= MAX_INSTRUCTIONS_BYTES && !instructions.contains('\0')
}

pub(crate) fn valid_text(text: &str, max_bytes: usize) -> bool {
    (1..=max_bytes).contains(&text.len()) && !text.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names_are_short_lowercase_identifiers() {
        for name in ["reviewer", "0day", "a", "code_review-2", &"a".repeat(64)] {
            assert!(valid_agent_name(name), "{name:?}");
        }
        for name in [
            "",
            "Reviewer",
            "_reviewer",
            "-reviewer",
            "re viewer",
            "réviewer",
            &"a".repeat(65),
        ] {
            assert!(!valid_agent_name(name), "{name:?}");
        }
    }

    #[test]
    fn instructions_may_be_empty_but_never_hold_nul_or_exceed_the_cap() {
        assert!(valid_instructions(""));
        assert!(valid_instructions(&"x".repeat(MAX_INSTRUCTIONS_BYTES)));
        assert!(!valid_instructions(&"x".repeat(MAX_INSTRUCTIONS_BYTES + 1)));
        assert!(!valid_instructions("a\0b"));
    }
}
