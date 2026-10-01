use unicode_properties::{GeneralCategory, GeneralCategoryGroup, UnicodeGeneralCategory};

pub(crate) fn is_punctuation_or_symbol(codepoint: char) -> bool {
    if codepoint.is_ascii() {
        return codepoint.is_ascii_graphic() && !codepoint.is_ascii_alphanumeric();
    }
    matches!(
        codepoint.general_category_group(),
        GeneralCategoryGroup::Punctuation | GeneralCategoryGroup::Symbol
    )
}

pub(crate) fn is_whitespace(codepoint: char) -> bool {
    if codepoint.is_ascii() {
        return matches!(codepoint, ' ' | '\t' | '\n' | '\u{0c}' | '\r');
    }
    codepoint.general_category() == GeneralCategory::SpaceSeparator
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flanking_classes_follow_unicode_general_categories() {
        assert!(is_punctuation_or_symbol('\u{2014}'));
        assert!(is_punctuation_or_symbol('\u{201C}'));
        assert!(is_punctuation_or_symbol('\u{00D7}'));
        assert!(is_punctuation_or_symbol('\u{FF0C}'));
        assert!(is_punctuation_or_symbol('\u{1F600}'));
        assert!(is_punctuation_or_symbol('\u{1F979}'));
        assert!(!is_punctuation_or_symbol('\u{00B5}'));
        assert!(!is_punctuation_or_symbol('\u{00AA}'));
        assert!(!is_punctuation_or_symbol('\u{3031}'));
        assert!(!is_punctuation_or_symbol('\u{4E2D}'));
        assert!(!is_punctuation_or_symbol('\u{2460}'));
        assert!(is_punctuation_or_symbol('*') && !is_punctuation_or_symbol('a'));
        assert!(
            is_whitespace('\u{00A0}') && is_whitespace('\u{3000}') && is_whitespace('\u{2003}')
        );
        assert!(is_whitespace(' ') && is_whitespace('\t') && !is_whitespace('\u{200B}'));
    }
}
