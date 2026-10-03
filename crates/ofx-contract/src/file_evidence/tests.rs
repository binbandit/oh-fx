use super::*;

fn evidence(path: &str, action: FileEvidenceAction, status: ToolResultStatus) -> FileEvidence {
    FileEvidence {
        path: path.to_owned(),
        new_path: None,
        tool_call_id: "call_1".to_owned(),
        tool_name: "read_file".to_owned(),
        action,
        status,
        model_view_covers_full_file: false,
        stale: false,
    }
}

#[test]
fn the_evidence_message_reads_as_upstream_writes_it() {
    let mut whole = evidence(
        "src/lib.rs",
        FileEvidenceAction::Read,
        ToolResultStatus::Success,
    );
    whole.model_view_covers_full_file = true;
    whole.stale = true;
    let mut renamed = evidence(
        "old.rs",
        FileEvidenceAction::Rename,
        ToolResultStatus::Failure,
    );
    renamed.new_path = Some("new.rs".to_owned());
    renamed.tool_name = "shell".to_owned();
    let search = FileEvidence {
        tool_name: "grep_files".to_owned(),
        ..evidence(".", FileEvidenceAction::Search, ToolResultStatus::Success)
    };
    assert_eq!(
        file_evidence_context(&[whole, renamed, search]),
        "Session file evidence from previous tool execution. Re-read stale paths before relying on exact contents:\n\
         - action=read status=success path=src/lib.rs model_view=full stale=true tool=read_file\n\
         - action=rename status=failure path=old.rs new_path=new.rs tool=shell\n\
         - action=search status=success path=. tool=grep_files"
    );
}

#[test]
fn every_action_has_upstreams_name() {
    let names: Vec<&str> = FileEvidenceAction::ALL
        .into_iter()
        .map(FileEvidenceAction::label)
        .collect();
    assert_eq!(
        names,
        [
            "read", "write", "edit", "delete", "rename", "copy", "search", "list", "unknown"
        ]
    );
}
