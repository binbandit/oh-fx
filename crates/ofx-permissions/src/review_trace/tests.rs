use std::path::PathBuf;

use ofx_contract::{CommandProfile, FileMutation, FileMutationState};

use super::*;

#[test]
fn review_actions_use_upstreams_kind_tags() {
    let call = ToolCall::new("call_1", "read_file", "{}");
    let run = CommandRequest::Run {
        command: "true".to_owned(),
        cwd: PathBuf::from("/work"),
        profile: CommandProfile::User,
        shell: None,
        terminal: false,
        reload: false,
    };
    let input = CommandRequest::SendInput {
        input: "y".to_owned(),
    };
    let mutation = FileMutation {
        target: PathBuf::from("/work/a.txt"),
        state: FileMutationState::Changes,
    };
    let kinds = [
        GatedAction::Command(&run),
        GatedAction::Command(&input),
        GatedAction::Command(&CommandRequest::Observe),
        GatedAction::Command(&CommandRequest::Stop),
        GatedAction::FileMutation(&mutation),
        GatedAction::Call(&call),
        GatedAction::McpTool(&call),
    ]
    .map(action_kind);
    assert_eq!(
        kinds,
        [
            "command",
            "shell_input",
            "tool",
            "tool",
            "file_mutation",
            "tool",
            "tool"
        ]
    );
}
