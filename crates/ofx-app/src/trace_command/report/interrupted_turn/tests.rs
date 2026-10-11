use ofx_contract::ToolCall;

use ofx_text::mask_secrets;

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

const SECRET_FORMS: [(&str, &str, &str); 4] = [
    (
        "https://deploy:",
        "hunter2secretvalue",
        "@example.test/path",
    ),
    (
        "Authorization: Bearer ",
        "abcdefghijklmnopqrstuvwxyz0123456789",
        " end",
    ),
    ("API_KEY=", "supersecretvalue1234", " next"),
    ("{\"api_key\":\"", "supersecretvalue1234", "\"}"),
];

#[test]
fn arguments_are_masked_before_they_are_cut() {
    for (before, secret, after) in SECRET_FORMS {
        for kept in 0..=secret.len() {
            let padding = "x".repeat(ARGUMENT_BYTES - before.len() - kept);
            let arguments = format!("{padding}{before}{secret}{after}");
            let written = section(Some(&turn(&arguments, &[])));
            let (shown, left_out) = masked_prefix(&arguments, ARGUMENT_BYTES);
            assert!(shown.len() <= ARGUMENT_BYTES);
            assert!(
                !written.contains(&secret[..4]),
                "{before}{secret} cut after {kept} secret bytes: {}",
                &written[written.len().saturating_sub(120)..]
            );
            assert_eq!(
                written.contains("... (truncated, "),
                left_out > 0,
                "{written}"
            );
        }
    }
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
