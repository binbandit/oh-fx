use ofx_contract::CommandProcessPresentation;
use serde::Serialize;

use super::*;
use crate::json_fields::parse_json;

#[derive(Serialize)]
struct InFrame(#[serde(with = "frame")] Option<CommandProcessPresentation>);

#[derive(Serialize)]
struct InCheckpoint(#[serde(with = "checkpoint")] Option<CommandProcessPresentation>);

const FORMS: [(CommandProcessPresentation, &str, &str); 4] = [
    (
        CommandProcessPresentation::ExitCode(-3),
        r#"{"exit_code":-3}"#,
        r#"{"kind":"exit_code","value":-3}"#,
    ),
    (
        CommandProcessPresentation::Signal(15),
        r#"{"signal":15}"#,
        r#"{"kind":"signal","value":15}"#,
    ),
    (
        CommandProcessPresentation::TimedOut,
        r#"{"timed_out":{}}"#,
        r#"{"kind":"timed_out","value":null}"#,
    ),
    (
        CommandProcessPresentation::OutputCaptureFailed,
        r#"{"output_capture_failed":{}}"#,
        r#"{"kind":"output_capture_failed","value":null}"#,
    ),
];

#[test]
fn frames_and_checkpoints_write_upstreams_two_forms_and_read_them_back() {
    for (presentation, in_frame, in_checkpoint) in FORMS {
        assert_eq!(
            serde_json::to_string(&InFrame(Some(presentation))).unwrap(),
            in_frame
        );
        assert_eq!(
            serde_json::to_string(&InCheckpoint(Some(presentation))).unwrap(),
            in_checkpoint
        );
        assert_eq!(
            frame::read(parse_json(in_frame.as_bytes()).unwrap()),
            Some(presentation)
        );
        assert_eq!(
            checkpoint::read(parse_json(in_checkpoint.as_bytes()).unwrap()),
            Some(presentation)
        );
        assert_eq!(
            frame::read(parse_json(in_checkpoint.as_bytes()).unwrap()),
            None
        );
        assert_eq!(
            checkpoint::read(parse_json(in_frame.as_bytes()).unwrap()),
            None
        );
    }
    assert_eq!(serde_json::to_string(&InFrame(None)).unwrap(), "null");
    assert_eq!(serde_json::to_string(&InCheckpoint(None)).unwrap(), "null");
}

#[test]
fn malformed_presentations_are_refused() {
    for text in [
        "null",
        "{}",
        r#"{"exit_code":1,"signal":2}"#,
        r#"{"exited":1}"#,
        r#"{"exit_code":1.5}"#,
        r#"{"exit_code":"1"}"#,
        r#"{"signal":-1}"#,
        r#"{"signal":4294967296}"#,
        r#"{"timed_out":null}"#,
        r#"{"timed_out":{"after":1}}"#,
        "[]",
    ] {
        assert_eq!(
            frame::read(parse_json(text.as_bytes()).unwrap()),
            None,
            "{text}"
        );
    }
    for text in [
        "null",
        r#"{"kind":"exit_code"}"#,
        r#"{"kind":"exit_code","value":1,"extra":true}"#,
        r#"{"kind":"exit_code","value":null}"#,
        r#"{"kind":"signal","value":4294967296}"#,
        r#"{"kind":"timed_out","value":1}"#,
        r#"{"kind":"output_capture_failed","value":{}}"#,
        r#"{"kind":"exited","value":1}"#,
        r#"{"kind":1,"value":1}"#,
    ] {
        assert_eq!(
            checkpoint::read(parse_json(text.as_bytes()).unwrap()),
            None,
            "{text}"
        );
    }
}
