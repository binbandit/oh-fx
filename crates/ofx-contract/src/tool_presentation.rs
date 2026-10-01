use crate::tool_args::parse_tool_args_object;
use crate::tool_dispatch::CallPresentation;

pub fn format_plain_action(
    tool_name: &str,
    presentation: &CallPresentation,
    arguments: &str,
) -> String {
    let Ok(arguments) = parse_tool_args_object(arguments) else {
        return format!("Working: {tool_name}");
    };
    let value = arguments
        .optional_string(presentation.label_argument)
        .unwrap_or(presentation.label_default);
    format!("{} {value}", presentation.action_label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_dispatch::ToolActivity;

    const READ: CallPresentation = CallPresentation {
        activity: ToolActivity::Read,
        action_label: "Reading",
        label_argument: "path",
        label_default: "file",
    };

    #[test]
    fn tool_presentation_preserves_plain_action_fallbacks() {
        let cases = [
            (r#"{"path":"src/main.zig"}"#, "Reading src/main.zig"),
            (r#"{"path":1}"#, "Reading file"),
            ("{}", "Reading file"),
            (r#"{"path":""}"#, "Reading "),
            (r#"{"path":" a "}"#, "Reading  a "),
            ("[]", "Working: read_file"),
            ("{", "Working: read_file"),
            (r#"{"path":"a.txt","line_count":1e400}"#, "Reading a.txt"),
            (r#"{"path":"a.txt","path":"b.txt"}"#, "Working: read_file"),
        ];
        for (arguments, expected) in cases {
            assert_eq!(
                format_plain_action("read_file", &READ, arguments),
                expected,
                "{arguments}"
            );
        }
    }
}
