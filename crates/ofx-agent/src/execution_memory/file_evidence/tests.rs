use super::*;

fn result(call: &ToolCall, status: ToolResultStatus, whole: bool) -> StepResult<'_> {
    StepResult {
        call_id: call.id.as_str(),
        tool_name: &call.name,
        output: "output",
        output_bytes: 6,
        status,
        process: None,
        review_feedback: false,
        permission_feedback: Vec::new(),
        model_view_covers_full_file: whole,
    }
}

fn step<'a>(calls: &'a [ToolCall], results: Vec<StepResult<'a>>) -> HistoryStep<'a> {
    HistoryStep {
        assistant: "",
        provider_replay: None,
        tool_calls: calls,
        tool_results: results,
    }
}

fn described(file: &FileEvidence) -> String {
    let status = match file.status {
        ToolResultStatus::Success => "success",
        ToolResultStatus::Failure => "failure",
    };
    let whole = if file.model_view_covers_full_file {
        " full"
    } else {
        ""
    };
    let stale = if file.stale { " stale" } else { "" };
    format!(
        "{} {} {} {} {status}{whole}{stale}",
        file.action.label(),
        file.path,
        file.tool_call_id,
        file.tool_name
    )
}

#[test]
fn file_tools_leave_upstreams_evidence_and_later_changes_make_reads_stale() {
    use ToolResultStatus::{Failure, Success};
    let first = [
        ToolCall::new("call_read", "read_file", r#"{"path":"src/lib.rs"}"#),
        ToolCall::new(
            "call_part",
            "read_file",
            r#"{"path":"README.md","start_line":5}"#,
        ),
        ToolCall::new(
            "call_grep",
            "grep_files",
            r#"{"pattern":"fn","path":"src"}"#,
        ),
        ToolCall::new("call_glob", "glob_files", r#"{"pattern":"*.rs","path":""}"#),
        ToolCall::new("call_shell", "shell", r#"{"command":"ls"}"#),
        ToolCall::new("call_missing", "read_file", r#"{"path":"gone.rs"}"#),
        ToolCall::new("call_bad", "read_file", "not json"),
    ];
    let second = [
        ToolCall::new(
            "call_edit",
            "edit_file",
            r#"{"path":"src/lib.rs","old_string":"a","new_string":"b"}"#,
        ),
        ToolCall::new(
            "sk-proj-0123456789abcdefghij",
            "write_file",
            r#"{"path":"README.md","content":"x"}"#,
        ),
    ];
    let steps = [
        step(
            &first,
            vec![
                result(&first[0], Success, true),
                result(&first[1], Success, false),
                result(&first[2], Success, true),
                result(&first[3], Success, false),
                result(&first[4], Success, false),
                result(&first[5], Failure, true),
                result(&first[6], Failure, false),
            ],
        ),
        step(
            &second,
            vec![
                result(&second[0], Success, false),
                result(&second[1], Failure, false),
            ],
        ),
    ];
    let files: Vec<String> = EarlierEvidence::default()
        .turn_files(&steps)
        .iter()
        .map(described)
        .collect();
    assert_eq!(
        files,
        [
            "read src/lib.rs call_read read_file success full stale",
            "read README.md call_part read_file success",
            "search src call_grep grep_files success",
            "search . call_glob glob_files success",
            "read gone.rs call_missing read_file failure",
            "edit src/lib.rs call_edit edit_file success",
            "write README.md redacted-94ef11c05501c8117ead8adf write_file failure",
        ]
    );
}

#[test]
fn recovered_evidence_goes_first_keeps_its_view_and_goes_stale_after_a_later_write() {
    for write_id in ["call_2", "call_1"] {
        let read = FileEvidence {
            path: "a.rs".to_owned(),
            new_path: None,
            tool_call_id: "call_1".to_owned(),
            tool_name: "read_file".to_owned(),
            action: FileEvidenceAction::Read,
            status: ToolResultStatus::Success,
            model_view_covers_full_file: true,
            stale: false,
        };
        let calls = [
            ToolCall::new("call_1", "read_file", r#"{"path":"a.rs"}"#),
            ToolCall::new(write_id, "write_file", r#"{"path":"a.rs","content":"x"}"#),
        ];
        let steps = [
            step(
                &calls[..1],
                vec![result(&calls[0], ToolResultStatus::Success, false)],
            ),
            step(
                &calls[1..],
                vec![result(&calls[1], ToolResultStatus::Success, false)],
            ),
        ];
        let expected = [
            "read a.rs call_1 read_file success full stale".to_owned(),
            format!("write a.rs {write_id} write_file success"),
        ];
        let described =
            |files: Vec<FileEvidence>| -> Vec<String> { files.iter().map(described).collect() };
        let earlier = EarlierEvidence::recovered(vec![read.clone()], 1);
        assert_eq!(described(earlier.turn_files(&steps)), expected);
        for cut in 1..=2 {
            let mut earlier = EarlierEvidence::recovered(vec![read.clone()], 1);
            earlier.keep_compacted(&steps[..cut]);
            assert_eq!(
                described(earlier.turn_files(&steps[cut..])),
                expected,
                "cut={cut} write={write_id}"
            );
        }
    }
}
