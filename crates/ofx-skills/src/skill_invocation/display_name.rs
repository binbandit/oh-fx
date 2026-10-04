use crate::skill_contract::{MAX_NAME_BYTES, invalid_skill_name_cause};
use crate::skill_invocation::failures::DISCOVERY_MODEL_NOTICE;

const CONTENT_NAME_PREFIX: &str = "<skill_content name=\"";
const ENCODED_BYTES_PER_NAME_BYTE: usize = 6;
const NAME_ENDINGS: [&str; 2] = ["\" location=\"", "\" resource=\""];
const ENTITIES: [(&str, &str); 7] = [
    ("&amp;", "&"),
    ("&lt;", "<"),
    ("&gt;", ">"),
    ("&quot;", "\""),
    ("&#x85;", "\u{85}"),
    ("&#x2028;", "\u{2028}"),
    ("&#x2029;", "\u{2029}"),
];

pub fn display_name_from_output(output: &str) -> Option<String> {
    let content = output
        .strip_prefix(DISCOVERY_MODEL_NOTICE)
        .unwrap_or(output);
    let rest = content.strip_prefix(CONTENT_NAME_PREFIX)?;
    let window = &rest.as_bytes()[..rest
        .len()
        .min(MAX_NAME_BYTES * ENCODED_BYTES_PER_NAME_BYTE + 1)];
    let end = window.iter().position(|byte| *byte == b'"')?;
    if !NAME_ENDINGS
        .iter()
        .any(|ending| rest[end..].starts_with(ending))
    {
        return None;
    }
    let mut encoded = &rest[..end];
    let mut name = String::new();
    while let Some(next) = encoded.chars().next() {
        let decoded = if next == '&' {
            let (entity, decoded) = ENTITIES
                .iter()
                .find(|(entity, _)| encoded.starts_with(entity))?;
            encoded = &encoded[entity.len()..];
            decoded
        } else if next == '<' || next == '>' {
            return None;
        } else {
            let literal = &encoded[..next.len_utf8()];
            encoded = &encoded[next.len_utf8()..];
            literal
        };
        if decoded.len() > MAX_NAME_BYTES - name.len() {
            return None;
        }
        name.push_str(decoded);
    }
    invalid_skill_name_cause(name.as_bytes())
        .is_none()
        .then_some(name)
}

#[cfg(test)]
mod tests {
    use ofx_text::write_scalar;

    use super::{
        CONTENT_NAME_PREFIX, DISCOVERY_MODEL_NOTICE, MAX_NAME_BYTES, display_name_from_output,
    };
    const RESULT_PREVIEW_BYTES: usize = 4 * 1024;

    fn preview(text: &str) -> &str {
        &text[..text.floor_char_boundary(RESULT_PREVIEW_BYTES)]
    }

    #[test]
    fn skill_display_names_round_trip_the_generated_result_header_within_its_preview() {
        let longest = "\"".repeat(MAX_NAME_BYTES);
        let names = [
            "workflow",
            "quotes\" & <name> ' café",
            "literal &quot; &amp;",
            "line\u{0085}\u{2028}\u{2029}break",
            longest.as_str(),
        ];
        for name in names {
            for with_notice in [false, true] {
                let mut out = String::new();
                if with_notice {
                    out.push_str(DISCOVERY_MODEL_NOTICE);
                }
                out.push_str(CONTENT_NAME_PREFIX);
                write_scalar(&mut out, name);
                out.push_str("\" location=\"/skills/");
                out.push_str(&"long-path".repeat(600));
                assert_eq!(
                    display_name_from_output(preview(&out)).as_deref(),
                    Some(name),
                    "{name:?} {with_notice}"
                );
            }
        }
    }

    #[test]
    fn skill_display_names_reject_unrelated_malformed_and_invalid_metadata() {
        let too_long = format!(
            "<skill_content name=\"{}\" location=\"/skills/workflow\">",
            "a".repeat(MAX_NAME_BYTES + 1)
        );
        let invalid = [
            "A quoted result: <skill_content name=\"workflow\" location=\"/skills/workflow\">",
            "<skill_content>body <skill_content name=\"workflow\" location=\"/skills/workflow\">",
            "<skill_content name=\"unfinished",
            "<skill_content name=\"workflow\">",
            "<skill_content name=\"\" location=\"/skills/workflow\">",
            "<skill_content name=\"bad&unknown;\" location=\"/skills/workflow\">",
            "<skill_content name=\"bad<value\" location=\"/skills/workflow\">",
            "<skill_content name=\"bad&#x0a;value\" location=\"/skills/workflow\">",
            "<skill_content name=\"../escape\" location=\"/skills/workflow\">",
            too_long.as_str(),
        ];
        for output in invalid {
            assert_eq!(display_name_from_output(output), None, "{output:?}");
        }
    }

    #[test]
    fn skill_display_name_parsing_stays_bounded() {
        let corpus = [
            format!("{CONTENT_NAME_PREFIX}workflow\" location=\"/skills/workflow\">"),
            format!(
                "{DISCOVERY_MODEL_NOTICE}{CONTENT_NAME_PREFIX}name&amp;value\" resource=\"SKILL.md\">"
            ),
            format!("{CONTENT_NAME_PREFIX}bad&unknown;\" location=\"/skills/workflow\">"),
        ];
        assert_eq!(
            display_name_from_output(&corpus[0]).as_deref(),
            Some("workflow")
        );
        assert_eq!(
            display_name_from_output(&corpus[1]).as_deref(),
            Some("name&value")
        );
        for input in &corpus {
            for end in (0..=input.len()).filter(|end| input.is_char_boundary(*end)) {
                if let Some(name) = display_name_from_output(&input[..end]) {
                    assert!(name.len() <= MAX_NAME_BYTES, "{name:?}");
                    assert!(!name.is_empty() && !name.contains(['/', '\\', '<', '>']));
                }
            }
        }
    }
}
