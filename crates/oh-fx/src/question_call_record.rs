use ofx_contract::{ToolArgValue, parse_tool_args_nested};

const QUESTION_TOOL: &str = "ask_user_question";
const QUESTION_TEXT_LEVELS: usize = 3;
const MAX_QUESTION_TEXT_BYTES: usize = 256;
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];

pub(crate) fn question_text(tool_name: &str, arguments: &str) -> Option<String> {
    if tool_name != QUESTION_TOOL {
        return None;
    }
    let arguments = parse_tool_args_nested(arguments, QUESTION_TEXT_LEVELS).ok()?;
    let Some(ToolArgValue::Array(questions)) = arguments.get("questions") else {
        return None;
    };
    let Some(ToolArgValue::Object(first)) = questions.first() else {
        return None;
    };
    let text = first.optional_string("question")?.trim_matches(TRIMMED);
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
    fn the_first_question_is_read_whatever_numbers_and_nesting_the_arguments_hold() {
        let deep = format!("{}0{}", "[".repeat(200), "]".repeat(200));
        let arguments = format!(
            r#"{{"questions":[{{"question":"Handle?","weight":1e400,"options":{deep}}}],"extra":-1e400}}"#
        );
        assert_eq!(
            question_text("ask_user_question", &arguments).as_deref(),
            Some("Handle?")
        );
        let arguments = format!(r#"{{"questions":[{{"question":{deep}}}]}}"#);
        assert_eq!(question_text("ask_user_question", &arguments), None);
        let arguments =
            format!(r#"{{"questions":[{{"question":"a"}}],"extra":[{{"b":{deep},"b":1}}]}}"#);
        assert_eq!(question_text("ask_user_question", &arguments), None);
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
