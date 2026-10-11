use super::*;

const WHITESPACE: [char; 25] = [
    '\u{9}', '\u{a}', '\u{b}', '\u{c}', '\u{d}', ' ', '\u{a0}', '\u{1680}', '\u{2000}', '\u{2001}',
    '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}',
    '\u{200a}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}', '\u{feff}',
];

fn limits(depth: usize, states: usize, repeat: usize, steps: usize) -> Limits {
    Limits {
        depth,
        states,
        repeat,
        steps,
    }
}

fn roomy() -> Limits {
    limits(64, 256, 64, 10_000)
}

#[test]
fn bounded_pattern_matching_covers_the_json_schema_interoperability_subset() {
    let cases = [
        ("^a*$", "aaa", true),
        ("^a*$", "abc", false),
        ("a+", "xxaayy", true),
        ("f.*o", "foo", true),
        ("[0-9]{2,}", "v12", true),
        ("^(ab|cd)?$", "cd", true),
        ("^\\p{Letter}+$", "\u{3c0}", true),
        ("^\\p{Letter}+$", "123", false),
        ("^[^a-c]$", "d", true),
        ("^[^a-c]$", "b", false),
        ("^a{2}$", "aa", true),
        ("^a{2,3}?$", "aaaa", false),
        ("^\\u{1F600}$", "\u{1F600}", true),
        ("^\\x41\\u0042$", "AB", true),
        ("^.$", "\n", false),
        ("^\\P{L}$", "1", true),
    ];
    for (pattern, text, expected) in cases {
        assert_eq!(
            matches(pattern, text, roomy()),
            Ok(expected),
            "{pattern} {text}"
        );
    }
}

#[test]
fn ecmascript_whitespace_classification_covers_exact_codepoint_boundaries() {
    for codepoint in WHITESPACE {
        assert!(is_ecmascript_whitespace(codepoint), "{codepoint:?}");
    }
    let non_whitespace = [
        '\u{8}', '\u{e}', '\u{1f}', '\u{21}', '\u{85}', '\u{9f}', '\u{a1}', '\u{167f}', '\u{1681}',
        '\u{180e}', '\u{1fff}', '\u{200b}', '\u{2027}', '\u{202a}', '\u{202e}', '\u{2030}',
        '\u{205e}', '\u{2060}', '\u{2fff}', '\u{3001}', '\u{fefe}', '\u{ff00}',
    ];
    for codepoint in non_whitespace {
        assert!(!is_ecmascript_whitespace(codepoint), "{codepoint:?}");
    }
}

#[test]
fn ecmascript_whitespace_escapes_share_exact_standalone_and_class_semantics() {
    let patterns = [
        "^\\s$", "^\\S$", "^[\\s]$", "^[^\\s]$", "^[\\S]$", "^[^\\S]$",
    ];
    let whitespace_results = [true, false, true, false, false, true];
    let non_whitespace_results = [false, true, false, true, true, false];
    for codepoint in WHITESPACE {
        let text = codepoint.to_string();
        for (pattern, expected) in patterns.iter().zip(whitespace_results) {
            assert_eq!(matches(pattern, &text, roomy()), Ok(expected), "{pattern}");
        }
    }
    let non_whitespace = [
        '\u{8}', '\u{e}', '\u{21}', '\u{85}', '\u{180e}', '\u{200b}', '\u{202a}', '\u{2060}',
        '\u{3001}', '\u{ff00}',
    ];
    for codepoint in non_whitespace {
        let text = codepoint.to_string();
        for (pattern, expected) in patterns.iter().zip(non_whitespace_results) {
            assert_eq!(matches(pattern, &text, roomy()), Ok(expected), "{pattern}");
        }
    }
    for text in ["\u{a0}", "\u{feff}", "\u{2003}", "\u{2028}", "\u{2029}"] {
        assert_eq!(matches("^\\S+$", text, roomy()), Ok(false));
    }
}

#[test]
fn pattern_compilation_and_matching_reject_unsupported_or_over_budget_behavior() {
    let tight = limits(4, 32, 8, 64);
    for source in [
        "(?=a)",
        "(a)\\1",
        "^\\cC$",
        "^\\cc$",
        "^\\p{digit}+$",
        "\\q",
        "[\\q]",
        "\\00",
        "[\\00]",
        "]",
        "}",
        "[\\s-a]",
        "[\\w-a]",
        "^*",
        "$+",
    ] {
        assert_eq!(
            validate(source, tight),
            Err(PatternError::InvalidPattern),
            "{source}"
        );
    }
    assert_eq!(validate("[\\s-]", tight), Ok(()));
    assert_eq!(validate("(^)?", tight), Ok(()));
    assert_eq!(
        validate("a{9}", tight),
        Err(PatternError::PatternLimitExceeded)
    );
    assert_eq!(validate("((((a))))", tight), Ok(()));
    assert_eq!(
        validate("(((((a)))))", tight),
        Err(PatternError::PatternLimitExceeded)
    );
    assert_eq!(matches("^[\\0]$", "\0", tight), Ok(true));
    assert_eq!(
        matches("(a|aa)*b", "aaaaaaaaaaaaaaaa", tight),
        Err(PatternError::PatternLimitExceeded)
    );
}

#[test]
fn hexadecimal_escapes_read_digits_as_upstream_integer_parsing_does() {
    assert_eq!(matches("^\\x+1$", "\u{1}", roomy()), Ok(true));
    assert_eq!(matches("^\\u0_41$", "A", roomy()), Ok(true));
    assert_eq!(matches("^\\x-0$", "\0", roomy()), Ok(true));
    for source in [
        "\\x-1",
        "\\x_1",
        "\\x1_",
        "\\uD800",
        "\\u{110000}",
        "\\u{}",
        "\\x4",
    ] {
        assert_eq!(
            validate(source, roomy()),
            Err(PatternError::InvalidPattern),
            "{source}"
        );
    }
}

#[test]
fn quantifier_bounds_and_lazy_suffixes_follow_the_subset() {
    assert_eq!(
        validate("a{3,2}", roomy()),
        Err(PatternError::InvalidPattern)
    );
    assert_eq!(validate("a**", roomy()), Err(PatternError::InvalidPattern));
    assert_eq!(validate("a*?", roomy()), Ok(()));
    assert_eq!(validate("a{2,}", roomy()), Ok(()));
    assert_eq!(
        validate("a{,2}", roomy()),
        Err(PatternError::InvalidPattern)
    );
    assert_eq!(
        validate("a{99999999999999999999999}", roomy()),
        Err(PatternError::PatternLimitExceeded)
    );
    assert_eq!(
        validate("[z-a]", roomy()),
        Err(PatternError::InvalidPattern)
    );
    assert_eq!(validate("[a", roomy()), Err(PatternError::InvalidPattern));
    assert_eq!(validate("(a", roomy()), Err(PatternError::InvalidPattern));
    assert_eq!(
        validate("\\\u{3c0}", roomy()),
        Err(PatternError::InvalidPattern)
    );
    assert_eq!(matches("^[]a]$", "]", roomy()), Ok(true));
}
