use ofx_contract::ToolCall;

use super::*;

const SECRET: &str = "sk-abcdefghijklmnopqrstuvwxyz0123456789";

fn section(turn: Option<&InterruptedTurn>) -> String {
    let mut out = String::new();
    write_section(&mut out, turn).unwrap();
    out
}

fn problems(turn: Option<&InterruptedTurn>) -> (usize, String) {
    let mut out = String::new();
    let count = write_problems(&mut out, turn).unwrap();
    (count, out)
}

fn turn(arguments: &str, completed: &[&str]) -> InterruptedTurn {
    InterruptedTurn {
        in_flight: Some(ToolCall::new("call_2", "shell", arguments)),
        completed_tools: completed.iter().map(|name| (*name).to_owned()).collect(),
    }
}

#[test]
fn nothing_is_written_without_an_interrupted_last_turn() {
    assert_eq!(section(None), "");
    assert_eq!(problems(None), (0, String::new()));
}

#[test]
fn the_section_names_the_in_flight_call_and_the_completed_tools() {
    let arguments = format!(r#"{{"command":"curl -H 'token {SECRET}' example.test"}}"#);
    let interrupted = turn(&arguments, &["read_file", "exa_search"]);
    let written = section(Some(&interrupted));
    assert_eq!(
        written,
        format!(
            "\n## Interrupted Turn\nlast turn was interrupted by user\n  in_flight_tool: shell\n  in_flight_args:\n    {}\n  completed_tools (2): read_file web_search\n",
            mask_secrets(&arguments)
        )
    );
    assert!(!written.contains(SECRET), "{written}");
    assert_eq!(
        problems(Some(&interrupted)),
        (
            1,
            "- interrupted turn in_flight_tool=shell completed_tools=2\n".to_owned()
        )
    );
}

#[test]
fn long_arguments_are_cut_at_a_character_boundary_and_counted() {
    let arguments = format!("{}é{}", "a".repeat(ARGUMENT_BYTES - 1), "b".repeat(10));
    let written = section(Some(&turn(&arguments, &[])));
    let shown = "a".repeat(ARGUMENT_BYTES - 1);
    assert!(
        written.ends_with(&format!(
            "  in_flight_args:\n    {shown}\n    ... (truncated, 12 more bytes)\n"
        )),
        "{written}"
    );
}

#[test]
fn a_turn_stopped_outside_a_tool_writes_only_its_marker() {
    let stopped = InterruptedTurn::default();
    assert_eq!(
        section(Some(&stopped)),
        "\n## Interrupted Turn\nlast turn was interrupted by user\n"
    );
    assert_eq!(
        problems(Some(&stopped)),
        (1, "- interrupted turn\n".to_owned())
    );
    let empty = turn("", &[]);
    assert_eq!(
        section(Some(&empty)),
        "\n## Interrupted Turn\nlast turn was interrupted by user\n  in_flight_tool: shell\n"
    );
}
