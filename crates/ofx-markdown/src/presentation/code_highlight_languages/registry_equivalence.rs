use std::fmt::Write;

use super::{KeywordCase, PROFILES, Profile, ProfileFlag, SOURCES};
use crate::{CodeBlockPayload, render_code_block_payload};

const SAMPLE: &str = "const let fn def SELECT FROM package import return true False null nil undefined 42 1.2 \"quoted\" 'single' `backtick` $name ${HOME} --flag && | # hash\n// slash\n/* block */ <!-- markup -->\n+added\n-removed\n@@ -1 +1 @@\n";

#[test]
fn registered_profile_handles_store_no_native_pointers() {
    assert!(size_of::<Profile>() <= size_of::<u16>());
}

#[test]
fn packed_registry_preserves_every_original_profile_field() {
    assert_eq!(PROFILES.len(), SOURCES.len());
    for (source, profile) in SOURCES.iter().zip(&PROFILES) {
        assert_eq!(profile.label(), source.label);
        assert_eq!(profile.quotes(), source.quotes, "{}", source.label);
        assert_eq!(profile.operators(), source.operators, "{}", source.label);
        assert_eq!(
            profile.block_comment(),
            source.block_comment,
            "{}",
            source.label
        );
        assert_eq!(
            profile.line_comments().collect::<Vec<_>>(),
            source.line_comments,
            "{}",
            source.label
        );
        assert_eq!(
            profile.keyword_case(),
            source.keyword_case,
            "{}",
            source.label
        );
        assert_eq!(profile.detection(), source.detection, "{}", source.label);
        for (actual, flag) in [
            (profile.dollar_vars(), ProfileFlag::DollarVars),
            (profile.dash_flags(), ProfileFlag::DashFlags),
            (profile.command_words(), ProfileFlag::CommandWords),
            (!profile.bare_numbers(), ProfileFlag::PlainBareNumbers),
            (profile.diff_lines(), ProfileFlag::DiffLines),
        ] {
            assert_eq!(
                actual,
                source.flags.contains(&flag),
                "{} {flag:?}",
                source.label
            );
        }
        for (original, packed) in [
            (source.aliases, profile.aliases()),
            (source.keywords, profile.keywords()),
            (source.literals, profile.literals()),
        ] {
            assert_eq!(
                packed.iter().collect::<Vec<_>>(),
                original.iter().collect::<Vec<_>>(),
                "{}",
                source.label
            );
            for bytes in original.iter() {
                let word = std::str::from_utf8(bytes).unwrap();
                for case in [KeywordCase::Sensitive, KeywordCase::AsciiInsensitive] {
                    for candidate in [
                        word.to_owned(),
                        word.to_ascii_uppercase(),
                        format!("{word}_"),
                        format!("_{word}"),
                    ] {
                        assert_eq!(
                            packed.contains(&candidate, case),
                            original.contains(&candidate, case),
                            "{} {candidate:?} {case:?}",
                            source.label
                        );
                    }
                }
            }
        }
        for alias in source.aliases.iter() {
            let alias = std::str::from_utf8(alias).unwrap();
            for label in [alias.to_owned(), alias.to_ascii_uppercase()] {
                assert_eq!(
                    super::resolve(&label).map(Profile::label),
                    Some(source.label)
                );
            }
        }
    }
}

#[test]
fn every_registered_language_retains_its_styled_output() {
    let mut output = String::new();
    for source in &SOURCES {
        let block = CodeBlockPayload {
            language: source.label.to_owned(),
            code: SAMPLE.to_owned(),
        };
        output.push_str(source.label);
        output.push('\n');
        writeln!(output, "{:?}", render_code_block_payload(&block)).expect("writing to a String");
    }
    assert_eq!(output, include_str!("profile_output.txt"));
}
