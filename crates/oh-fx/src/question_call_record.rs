use ofx_contract::parse_tool_args_object;
use serde_json::Value;

const QUESTION_TOOL: &str = "ask_user_question";
const MAX_QUESTION_TEXT_BYTES: usize = 256;
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];

pub(crate) fn question_text(tool_name: &str, arguments: &str) -> Option<String> {
    if tool_name != QUESTION_TOOL || parse_tool_args_object(arguments).is_err() {
        return None;
    }
    let arguments: Value = serde_json::from_str(arguments).ok()?;
    let text = arguments
        .get("questions")?
        .as_array()?
        .first()?
        .as_object()?
        .get("question")?
        .as_str()?
        .trim_matches(TRIMMED);
    (!text.is_empty()).then(|| text[..text.floor_char_boundary(MAX_QUESTION_TEXT_BYTES)].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_question_is_recorded_trimmed_for_question_calls_only() {
        let arguments = r#"{"questions":[{"question":" What is your GitHub handle?\n"},{"question":"Second?"}]}"#;
        assert_eq!(
            question_text("ask_user_question", arguments).as_deref(),
            Some("What is your GitHub handle?")
        );
        assert_eq!(question_text("read_file", arguments), None);
        for arguments in [
            "not-json",
            "[]",
            "{}",
            r#"{"questions":{}}"#,
            r#"{"questions":[]}"#,
            r#"{"questions":[1]}"#,
            r#"{"questions":[{"question":1}]}"#,
            r#"{"questions":[{"question":" \t"}]}"#,
            r#"{"questions":[{"question":"a"}],"questions":[]}"#,
        ] {
            assert_eq!(
                question_text("ask_user_question", arguments),
                None,
                "{arguments}"
            );
        }
    }

    #[test]
    fn long_question_text_is_clipped_at_a_character_boundary() {
        let question = format!("{}é", "a".repeat(255));
        let arguments = format!(r#"{{"questions":[{{"question":"{question}"}}]}}"#);
        assert_eq!(
            question_text("ask_user_question", &arguments),
            Some("a".repeat(255))
        );
        let exact = "b".repeat(300);
        let arguments = format!(r#"{{"questions":[{{"question":"{exact}"}}]}}"#);
        assert_eq!(
            question_text("ask_user_question", &arguments).map(|text| text.len()),
            Some(256)
        );
    }
}
