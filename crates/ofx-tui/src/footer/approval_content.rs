use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ofx_contract::{ApprovalRequest, CommandProfile, CommandRequest, SessionGrant};
use ofx_text::encode_terminal_safe;

use super::command_text::project_command_text;
use super::phrase::{PathText, Phrase};

const GENERIC_KIND: &str = "Tool";
const COMMAND_KIND: &str = "Command";
const GENERIC_QUESTION: &str = "Would you like to allow this action?";
const COMMAND_QUESTION: &str = "Would you like to run the following command?";
const GENERIC_REASON: &str = "This action needs approval before oh-fx can continue.";
const COMMAND_LEAD: &str = "$ ";
const INPUT_LEAD: &str = "> ";
const RUN_HEADER: &str = "# shell.run";
const REMEMBER_COMMAND: &str = "don't ask again for this exact command";
const FOR_THIS_SESSION: &str = " for this session";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApprovalContent {
    pub(crate) kind: &'static str,
    pub(crate) question: &'static str,
    pub(crate) reason: Option<String>,
    pub(crate) action: Vec<ActionBlock>,
    pub(crate) remember: Option<Phrase>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActionBlock {
    Line(Phrase),
    Wrapped { lead: &'static str, text: String },
}

impl ApprovalContent {
    pub(crate) fn from_request(request: &ApprovalRequest, workspace_root: &Path) -> Self {
        let remember = request.scope.always.as_ref().map(remember_label);
        match &request.command {
            Some(CommandRequest::Run {
                command,
                cwd,
                profile,
                shell,
                terminal,
            }) => Self {
                kind: COMMAND_KIND,
                question: COMMAND_QUESTION,
                reason: None,
                action: vec![ActionBlock::Wrapped {
                    lead: COMMAND_LEAD,
                    text: run_text(
                        command,
                        &RunSettings {
                            cwd: (cwd != workspace_root).then_some(cwd.as_path()),
                            profile: *profile,
                            shell: shell.as_deref(),
                            terminal: *terminal,
                        },
                    ),
                }],
                remember,
            },
            Some(CommandRequest::SendInput { input }) => Self::generic(
                vec![
                    title_line(request),
                    ActionBlock::Wrapped {
                        lead: INPUT_LEAD,
                        text: safe_text(input.as_bytes()),
                    },
                ],
                remember,
            ),
            Some(CommandRequest::Observe | CommandRequest::Stop) => {
                Self::generic(vec![title_line(request)], remember)
            }
            None => match &request.scope.target {
                Some(target) => Self::generic(
                    vec![ActionBlock::Line(Phrase::with_path(
                        format!("{} ", safe_text(request.tool_name.as_bytes())),
                        PathText::from_raw(target.as_os_str().as_bytes()),
                        "",
                    ))],
                    remember,
                ),
                None => Self::generic(vec![title_line(request)], remember),
            },
        }
    }

    fn generic(action: Vec<ActionBlock>, remember: Option<Phrase>) -> Self {
        Self {
            kind: GENERIC_KIND,
            question: GENERIC_QUESTION,
            reason: Some(GENERIC_REASON.to_owned()),
            action,
            remember,
        }
    }
}

fn title_line(request: &ApprovalRequest) -> ActionBlock {
    ActionBlock::Line(Phrase::plain(safe_text(request.title.as_bytes())))
}

struct RunSettings<'a> {
    cwd: Option<&'a Path>,
    profile: CommandProfile,
    shell: Option<&'a Path>,
    terminal: bool,
}

impl RunSettings<'_> {
    fn describe(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if self.profile == CommandProfile::Clean {
            parts.push("profile=clean".to_owned());
        }
        if let Some(cwd) = self.cwd {
            parts.push(format!("cwd={}", safe_text(cwd.as_os_str().as_bytes())));
        }
        if self.terminal {
            parts.push("tty=true".to_owned());
        }
        if let Some(shell) = self.shell {
            parts.push(format!("shell={}", safe_text(shell.as_os_str().as_bytes())));
        }
        parts
    }
}

fn run_text(command: &str, settings: &RunSettings<'_>) -> String {
    let command = project_command_text(command);
    let parts = settings.describe();
    if parts.is_empty() {
        command
    } else {
        format!("{RUN_HEADER} {}\n{command}", parts.join(" "))
    }
}

fn remember_label(grant: &SessionGrant) -> Phrase {
    match grant {
        SessionGrant::Command {
            profile,
            shell,
            terminal,
            ..
        } => {
            let parts = RunSettings {
                cwd: None,
                profile: *profile,
                shell: shell.as_deref(),
                terminal: *terminal,
            }
            .describe();
            if parts.is_empty() {
                Phrase::plain(REMEMBER_COMMAND)
            } else {
                Phrase::plain(format!("{REMEMBER_COMMAND} ({})", parts.join(", ")))
            }
        }
        SessionGrant::WorkspaceFiles => {
            Phrase::plain(format!("allow workspace file access{FOR_THIS_SESSION}"))
        }
        SessionGrant::FileChangesUnder(root) => under("allow file changes under ", root),
        SessionGrant::ReadsUnder(root) => under("allow reads under ", root),
        SessionGrant::GlobsUnder(root) => under("allow name searches under ", root),
        SessionGrant::GrepsUnder(root) => under("allow content searches under ", root),
    }
}

fn under(head: &str, root: &Path) -> Phrase {
    Phrase::with_path(
        head,
        PathText::from_raw(root.as_os_str().as_bytes()),
        FOR_THIS_SESSION,
    )
}

fn safe_text(raw: &[u8]) -> String {
    encode_terminal_safe(raw, usize::MAX).text
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ofx_contract::{ApprovalScope, PathAccess, RequestId};

    use super::*;

    fn request(command: CommandRequest, always: Option<SessionGrant>) -> ApprovalRequest {
        ApprovalRequest {
            id: RequestId::new(1),
            tool_name: "shell".to_owned(),
            title: "Running echo hi".to_owned(),
            tool_arguments_preview: String::new(),
            scope: ApprovalScope {
                target: None,
                access: PathAccess::WorkspaceOnly,
                always,
            },
            command: Some(command),
            file: None,
        }
    }

    fn run(command: &str, cwd: &str, profile: CommandProfile, terminal: bool) -> CommandRequest {
        CommandRequest::Run {
            command: command.to_owned(),
            cwd: PathBuf::from(cwd),
            profile,
            shell: None,
            terminal,
        }
    }

    fn grant(profile: CommandProfile, terminal: bool) -> SessionGrant {
        SessionGrant::Command {
            command: "echo hi".to_owned(),
            profile,
            shell: None,
            terminal,
        }
    }

    fn content(command: CommandRequest, always: Option<SessionGrant>) -> ApprovalContent {
        ApprovalContent::from_request(&request(command, always), Path::new("/ws"))
    }

    #[test]
    fn commands_show_their_full_text_and_offer_the_exact_command() {
        let shown = content(
            run("echo hi\n\x1b[31mdone", "/ws", CommandProfile::User, false),
            Some(grant(CommandProfile::User, false)),
        );
        assert_eq!(shown.kind, "Command");
        assert_eq!(shown.question, COMMAND_QUESTION);
        assert_eq!(shown.reason, None);
        assert_eq!(
            shown.action,
            [ActionBlock::Wrapped {
                lead: "$ ",
                text: "echo hi\n\\x1b[31mdone".to_owned()
            }]
        );
        assert_eq!(
            shown.remember,
            Some(Phrase::plain("don't ask again for this exact command"))
        );
    }

    #[test]
    fn commands_name_a_clean_profile_another_directory_a_terminal_and_a_named_shell() {
        let shown = content(
            run("make", "/tmp/a\nb", CommandProfile::Clean, true),
            Some(grant(CommandProfile::Clean, true)),
        );
        assert_eq!(
            shown.action,
            [ActionBlock::Wrapped {
                lead: "$ ",
                text: "# shell.run profile=clean cwd=/tmp/a\\x0ab tty=true\nmake".to_owned()
            }]
        );
        assert_eq!(
            shown.remember,
            Some(Phrase::plain(
                "don't ask again for this exact command (profile=clean, tty=true)"
            ))
        );
        let named = CommandRequest::Run {
            command: "top".to_owned(),
            cwd: PathBuf::from("/ws"),
            profile: CommandProfile::User,
            shell: Some(PathBuf::from("/opt/fish\x1b")),
            terminal: true,
        };
        let named_grant = SessionGrant::Command {
            command: "top".to_owned(),
            profile: CommandProfile::User,
            shell: Some(PathBuf::from("/opt/fish\x1b")),
            terminal: true,
        };
        let shown = content(named, Some(named_grant));
        assert_eq!(
            shown.action,
            [ActionBlock::Wrapped {
                lead: "$ ",
                text: "# shell.run tty=true shell=/opt/fish\\x1b\ntop".to_owned()
            }]
        );
        assert_eq!(
            shown.remember,
            Some(Phrase::plain(
                "don't ask again for this exact command (tty=true, shell=/opt/fish\\x1b)"
            ))
        );
        let plain = content(run("make", "/ws", CommandProfile::User, false), None);
        assert_eq!(plain.remember, None);
    }

    #[test]
    fn reads_and_searches_name_their_resolved_target_and_the_tree_they_grant() {
        let read = |tool: &str, always| ApprovalRequest {
            id: RequestId::new(1),
            tool_name: tool.to_owned(),
            title: "Reading ../workspace/../secret.txt".to_owned(),
            tool_arguments_preview: String::new(),
            scope: ApprovalScope {
                target: Some(PathBuf::from("/home/me/secret.txt")),
                access: PathAccess::Within(PathBuf::from("/home/me")),
                always,
            },
            command: None,
            file: None,
        };
        let root = || PathBuf::from("/home/me");
        for (tool, grant, label) in [
            (
                "read_file",
                SessionGrant::ReadsUnder(root()),
                "allow reads under ",
            ),
            (
                "glob_files",
                SessionGrant::GlobsUnder(root()),
                "allow name searches under ",
            ),
            (
                "grep_files",
                SessionGrant::GrepsUnder(root()),
                "allow content searches under ",
            ),
        ] {
            let shown = ApprovalContent::from_request(&read(tool, Some(grant)), Path::new("/ws"));
            assert_eq!(
                shown.action,
                [ActionBlock::Line(Phrase::with_path(
                    format!("{tool} "),
                    PathText::from_raw(b"/home/me/secret.txt"),
                    ""
                ))]
            );
            assert_eq!(
                shown.remember,
                Some(Phrase::with_path(
                    label,
                    PathText::from_raw(b"/home/me"),
                    " for this session"
                ))
            );
        }
    }

    #[test]
    fn file_change_grants_name_the_workspace_or_their_tree() {
        assert_eq!(
            remember_label(&SessionGrant::WorkspaceFiles),
            Phrase::plain("allow workspace file access for this session")
        );
        assert_eq!(
            remember_label(&SessionGrant::FileChangesUnder(PathBuf::from("/etc"))),
            Phrase::with_path(
                "allow file changes under ",
                PathText::from_raw(b"/etc"),
                " for this session"
            )
        );
    }

    #[test]
    fn input_for_a_running_command_is_shown_whole_with_controls_visible() {
        let shown = content(
            CommandRequest::SendInput {
                input: "yes\n\x03".to_owned(),
            },
            None,
        );
        assert_eq!(shown.kind, "Tool");
        assert_eq!(
            shown.action,
            [
                ActionBlock::Line(Phrase::plain("Running echo hi")),
                ActionBlock::Wrapped {
                    lead: "> ",
                    text: "yes\\x0a\\x03".to_owned()
                }
            ]
        );
    }
}
