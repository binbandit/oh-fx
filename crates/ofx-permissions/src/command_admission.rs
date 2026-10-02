use std::path::Path;

use ofx_contract::{Admission, CommandRequest, PathAccess, PermissionMode, ToolCall};
use ofx_shell::known_reversible_auto_command;
use ofx_workspace::path_inside;

const SHELL_TOOL: &str = "shell";

pub(crate) fn command_admission(
    mode: PermissionMode,
    workspace_root: &Path,
    request: &CommandRequest,
) -> Admission {
    match mode {
        PermissionMode::Yolo => Admission::Allowed(PathAccess::WorkspaceOrExternal),
        PermissionMode::Ask => Admission::ApprovalRequired,
        PermissionMode::Auto if runs_without_review(workspace_root, request) => {
            Admission::Allowed(PathAccess::WorkspaceOnly)
        }
        PermissionMode::Auto => Admission::ReviewRequired,
    }
}

pub(crate) fn undescribed_shell_call_admission(
    mode: PermissionMode,
    call: &ToolCall,
) -> Option<Admission> {
    (call.name == SHELL_TOOL).then_some(match mode {
        PermissionMode::Yolo => Admission::Allowed(PathAccess::WorkspaceOrExternal),
        PermissionMode::Ask => Admission::ApprovalRequired,
        PermissionMode::Auto => Admission::ReviewRequired,
    })
}

fn runs_without_review(workspace_root: &Path, request: &CommandRequest) -> bool {
    match request {
        CommandRequest::Run {
            command,
            cwd,
            terminal,
            ..
        } => {
            !terminal && path_inside(workspace_root, cwd) && known_reversible_auto_command(command)
        }
        CommandRequest::Observe => true,
        CommandRequest::SendInput { .. } | CommandRequest::Stop => false,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ofx_contract::{CommandProfile, ToolCallId};

    use super::*;

    const ALLOWED: Admission = Admission::Allowed(PathAccess::WorkspaceOnly);
    const WORKSPACE: &str = "/workspace";

    fn run(command: &str, cwd: &str, terminal: bool) -> CommandRequest {
        CommandRequest::Run {
            command: command.to_owned(),
            cwd: PathBuf::from(cwd),
            profile: CommandProfile::User,
            shell: None,
            terminal,
        }
    }

    fn admit(mode: PermissionMode, request: &CommandRequest) -> Admission {
        command_admission(mode, Path::new(WORKSPACE), request)
    }

    fn requests() -> Vec<CommandRequest> {
        vec![
            run("git status", WORKSPACE, false),
            run("rm -rf .", WORKSPACE, false),
            CommandRequest::Observe,
            CommandRequest::SendInput {
                input: "y\n".to_owned(),
            },
            CommandRequest::Stop,
        ]
    }

    #[test]
    fn full_access_runs_every_action_and_ask_mode_needs_approval_for_each() {
        for request in requests() {
            assert_eq!(
                admit(PermissionMode::Yolo, &request),
                Admission::Allowed(PathAccess::WorkspaceOrExternal),
                "{request:?}"
            );
            assert_eq!(
                admit(PermissionMode::Ask, &request),
                Admission::ApprovalRequired,
                "{request:?}"
            );
        }
    }

    #[test]
    fn auto_mode_runs_reversible_workspace_commands_and_observations_without_review() {
        for request in [
            run("git status --short", WORKSPACE, false),
            run("npm test", "/workspace/web", false),
            run("zig build test", WORKSPACE, false),
            CommandRequest::Observe,
        ] {
            assert_eq!(
                admit(PermissionMode::Auto, &request),
                ALLOWED,
                "{request:?}"
            );
        }
    }

    #[test]
    fn auto_mode_sends_everything_else_to_the_reviewer() {
        for request in [
            run("rm -rf .", WORKSPACE, false),
            run("pwd", WORKSPACE, false),
            run("git status", WORKSPACE, true),
            run("npm install", "/elsewhere/project", false),
            run("git status", "/workspace-other", false),
            run(
                "npm run review '&&' git status --script-shell=/not/a/shell",
                WORKSPACE,
                false,
            ),
            run(
                "npm install \\&\\& git status --prefix=/tmp/outside",
                WORKSPACE,
                false,
            ),
            CommandRequest::SendInput {
                input: "y\n".to_owned(),
            },
            CommandRequest::Stop,
        ] {
            assert_eq!(
                admit(PermissionMode::Auto, &request),
                Admission::ReviewRequired,
                "{request:?}"
            );
        }
    }

    #[test]
    fn shell_calls_that_do_not_describe_their_command_never_run_below_full_access() {
        let call = |name: &str| ToolCall {
            id: ToolCallId::new("call-1"),
            name: name.to_owned(),
            arguments: r#"{"request":{"action":"run","command":"git status"}}"#.to_owned(),
        };
        assert_eq!(
            undescribed_shell_call_admission(PermissionMode::Auto, &call("shell")),
            Some(Admission::ReviewRequired)
        );
        assert_eq!(
            undescribed_shell_call_admission(PermissionMode::Ask, &call("shell")),
            Some(Admission::ApprovalRequired)
        );
        assert_eq!(
            undescribed_shell_call_admission(PermissionMode::Yolo, &call("shell")),
            Some(Admission::Allowed(PathAccess::WorkspaceOrExternal))
        );
        assert_eq!(
            undescribed_shell_call_admission(PermissionMode::Auto, &call("read_file")),
            None
        );
    }
}
