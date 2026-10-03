use std::path::PathBuf;

use ofx_contract::{
    ActionLabel, ApprovalScope, CallDescription, CommandProfile, Concurrency, FileMutation,
    FileMutationState, PathAccess, RequestId, ToolActivity, ToolCallId, ToolEffect,
};

use super::*;

fn request(tool_name: &str, target: Option<&str>) -> ApprovalRequest {
    ApprovalRequest {
        id: RequestId::new(1),
        call_id: ToolCallId::new("call-1"),
        tool_name: tool_name.to_owned(),
        description: CallDescription {
            title: String::new(),
            label: target.map(|target| ActionLabel {
                active: "Reading",
                completed: "Read",
                target: target.to_owned(),
            }),
            activity: ToolActivity::Read,
            effect: ToolEffect::ReadOnly,
            concurrency: Concurrency::Serial,
        },
        tool_arguments_preview: String::new(),
        tool_arguments_truncated: false,
        scope: ApprovalScope {
            target: None,
            access: PathAccess::WorkspaceOnly,
            always: None,
        },
        command: None,
        file: None,
    }
}

fn shell(command: CommandRequest) -> ApprovalRequest {
    ApprovalRequest {
        command: Some(command),
        ..request("shell", None)
    }
}

fn run(command: &str) -> String {
    permission_label(&shell(CommandRequest::Run {
        command: command.to_owned(),
        cwd: PathBuf::from("/workspace"),
        profile: CommandProfile::User,
        shell: None,
        terminal: false,
    }))
}

#[test]
fn labels_follow_upstreams_permission_label_for_each_kind_of_call() {
    assert_eq!(
        permission_label(&request("read_file", Some("/etc/hosts"))),
        "read_file /etc/hosts"
    );
    assert_eq!(permission_label(&request("glob_files", None)), "glob_files");
    assert_eq!(
        permission_label(&ApprovalRequest {
            file: Some(FileMutation {
                target: PathBuf::from("/workspace/notes.txt"),
                state: FileMutationState::Changes,
            }),
            ..request("write_file", Some("notes.txt"))
        }),
        "file_mutation"
    );
    assert_eq!(
        permission_label(&shell(CommandRequest::SendInput {
            input: "q".to_owned()
        })),
        "shell interact"
    );
    assert_eq!(
        permission_label(&shell(CommandRequest::Observe)),
        "shell interact"
    );
    assert_eq!(permission_label(&shell(CommandRequest::Stop)), "shell stop");
}

#[test]
fn run_labels_carry_upstreams_risk_and_safer_notes() {
    assert_eq!(
        run("git reset --hard"),
        "shell.run git reset --hard (risk: command may discard version-control state; safer: inspect git status first and revert only the intended files)"
    );
    assert_eq!(
        run("cat notes.txt"),
        "shell.run cat notes.txt (safer: use read_file for file inspection)"
    );
    assert_eq!(run("cargo test"), "shell.run cargo test");
    let escaped = run("printf '\x1b[2J'");
    assert!(!escaped.contains('\x1b'), "{escaped}");
    assert_eq!(
        escaped,
        format!(
            "shell.run {}",
            encode_terminal_safe(b"printf '\x1b[2J'", 120).text
        )
    );
    let long = format!("echo {}", "x".repeat(200));
    let label = run(&long);
    assert!(label.starts_with("shell.run echo xxx"), "{label}");
    assert_eq!(
        label.len(),
        "shell.run ".len() + encode_terminal_safe(long.as_bytes(), 120).text.len()
    );
}

#[test]
fn only_a_single_y_approves_and_everything_else_denies() {
    for (input, expected) in [
        (&b"y\n"[..], ApprovalDecision::Once),
        (b"Y", ApprovalDecision::Once),
        (b" \ty\r\n", ApprovalDecision::Once),
        (b"yes\n", ApprovalDecision::Deny),
        (b"n\n", ApprovalDecision::Deny),
        (b"\n", ApprovalDecision::Deny),
        (b"", ApprovalDecision::Deny),
        (b"\x0by\n", ApprovalDecision::Deny),
    ] {
        assert_eq!(decision(&mut &input[..]), expected, "{input:?}");
    }
    let overlong = [b' '; 300];
    assert_eq!(decision(&mut &overlong[..]), ApprovalDecision::Deny);
    let mut longest = vec![b' '; 255];
    longest.push(b'y');
    longest.push(b'\n');
    assert_eq!(decision(&mut &longest[..]), ApprovalDecision::Once);
}
