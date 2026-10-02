use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ofx_contract::{
    ApprovalRequest, CommandProfile, CommandRequest, FileMutation, FileMutationState, SessionGrant,
};
use ofx_text::encode_terminal_safe;

use super::command_text::project_command_text;
use super::phrase::{PathText, Phrase};

const GENERIC_KIND: &str = "Tool";
const COMMAND_KIND: &str = "Command";
const GENERIC_QUESTION: &str = "Would you like to allow this action?";
const COMMAND_QUESTION: &str = "Would you like to run the following command?";
const GENERIC_REASON: &str = "This action needs approval before oh-fx can continue.";
const WORKSPACE_CHANGE_REASON: &str = "This action changes files in your workspace.";
const EXTERNAL_CHANGE_REASON: &str = "This action changes a file outside your workspace.";
const COMMAND_LEAD: &str = "$ ";
const INPUT_LEAD: &str = "> ";
const RUN_HEADER: &str = "# shell.run";
const REMEMBER_COMMAND: &str = "don't ask again for this exact command in ";
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
    Note(&'static str),
    Wrapped { lead: &'static str, text: String },
    Arguments { target: String, preview: String },
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
                reason: first_url_host(command).map(|host| {
                    format!(
                        "This command may make a network request to {}.",
                        safe_text(host.as_bytes())
                    )
                }),
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
            None => match (&request.file, &request.scope.target) {
                (Some(file), _) => Self::file_change(request, file, workspace_root, remember),
                (None, Some(target)) => {
                    Self::generic(vec![labelled_path(request, target)], remember)
                }
                (None, None) if !request.tool_arguments_preview.is_empty() => Self::generic(
                    vec![ActionBlock::Arguments {
                        target: safe_text(request.title.as_bytes()),
                        preview: request.tool_arguments_preview.clone(),
                    }],
                    remember,
                ),
                (None, None) => Self::generic(vec![title_line(request)], remember),
            },
        }
    }

    fn file_change(
        request: &ApprovalRequest,
        file: &FileMutation,
        workspace_root: &Path,
        remember: Option<Phrase>,
    ) -> Self {
        let (kind, question) = match request.tool_name.as_str() {
            "write_file" => (
                "Write file",
                "Would you like to create or update this file?",
            ),
            "edit_file" => ("Edit file", "Would you like to edit this file?"),
            _ => (GENERIC_KIND, GENERIC_QUESTION),
        };
        let reason = if file.target.starts_with(workspace_root) {
            WORKSPACE_CHANGE_REASON
        } else {
            EXTERNAL_CHANGE_REASON
        };
        Self {
            kind,
            question,
            reason: Some(reason.to_owned()),
            action: vec![
                labelled_path(request, &file.target),
                ActionBlock::Note(match file.state {
                    FileMutationState::Creates => {
                        "Creates this file. Its content is not previewed here."
                    }
                    FileMutationState::Changes => {
                        "Changes this file. The change is not previewed here."
                    }
                    FileMutationState::Unchanged => "Leaves this file unchanged.",
                    FileMutationState::Unread => {
                        "Writes this file without having read it. The change is not previewed here."
                    }
                }),
            ],
            remember,
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

fn labelled_path(request: &ApprovalRequest, path: &Path) -> ActionBlock {
    ActionBlock::Line(Phrase::with_path(
        format!("{} ", safe_text(request.tool_name.as_bytes())),
        PathText::from_raw(path.as_os_str().as_bytes()),
        "",
    ))
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
            cwd,
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
            let tail = if parts.is_empty() {
                String::new()
            } else {
                format!(" ({})", parts.join(", "))
            };
            Phrase::with_path(
                REMEMBER_COMMAND,
                PathText::from_raw(cwd.as_os_str().as_bytes()),
                &tail,
            )
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

fn first_url_host(text: &str) -> Option<&str> {
    for scheme in ["https://", "http://"] {
        let Some(index) = text.find(scheme) else {
            continue;
        };
        let start = index + scheme.len();
        if start >= text.len() {
            return None;
        }
        let rest = text[start..].trim_start_matches('/');
        let end = rest
            .find(['/', ':', '?', '#', ' ', '\t', '\n', '\r', '\'', '"', '`'])
            .unwrap_or(rest.len());
        if end > 0 {
            return Some(&rest[..end]);
        }
    }
    None
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
            cwd: PathBuf::from("/ws"),
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
            Some(Phrase::with_path(
                "don't ask again for this exact command in ",
                PathText::from_raw(b"/ws"),
                ""
            ))
        );
    }

    #[test]
    fn commands_name_the_first_host_they_may_contact() {
        let reason =
            |command: &str| content(run(command, "/ws", CommandProfile::User, false), None).reason;
        assert_eq!(
            reason("curl -I https://example.com/path?q=1").as_deref(),
            Some("This command may make a network request to example.com.")
        );
        assert_eq!(
            reason("wget 'http:///mirror.test:8080/x' https://second.test").as_deref(),
            Some("This command may make a network request to second.test.")
        );
        assert_eq!(
            reason("curl http://evil\x1b[31m.test/").as_deref(),
            Some("This command may make a network request to evil\\x1b[31m.test.")
        );
        assert_eq!(reason("zig build test"), None);
        assert_eq!(reason("echo https://"), None);
        assert_eq!(
            reason("echo https:// http://later.test").as_deref(),
            Some("This command may make a network request to later.test.")
        );
        assert_eq!(reason("echo http://later.test https://"), None);
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
            Some(Phrase::with_path(
                "don't ask again for this exact command in ",
                PathText::from_raw(b"/ws"),
                " (profile=clean, tty=true)"
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
            cwd: PathBuf::from("/ws/sub\x1b"),
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
            Some(Phrase::with_path(
                "don't ask again for this exact command in ",
                PathText::from_raw(b"/ws/sub\x1b"),
                " (tty=true, shell=/opt/fish\\x1b)"
            ))
        );
        assert_eq!(
            shown.remember.unwrap().fit(200).0,
            "don't ask again for this exact command in /ws/sub\\x1b (tty=true, shell=/opt/fish\\x1b)"
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
    fn file_changes_name_their_target_and_what_the_change_does() {
        let change = |tool: &str, target: &str, state| ApprovalRequest {
            id: RequestId::new(1),
            tool_name: tool.to_owned(),
            title: "Writing notes.md".to_owned(),
            tool_arguments_preview: String::new(),
            scope: ApprovalScope {
                target: None,
                access: PathAccess::WorkspaceOnly,
                always: None,
            },
            command: None,
            file: Some(FileMutation {
                target: PathBuf::from(target),
                state,
            }),
        };
        let shown = ApprovalContent::from_request(
            &change("write_file", "/ws/notes.md", FileMutationState::Creates),
            Path::new("/ws"),
        );
        assert_eq!(shown.kind, "Write file");
        assert_eq!(
            shown.question,
            "Would you like to create or update this file?"
        );
        assert_eq!(shown.reason.as_deref(), Some(WORKSPACE_CHANGE_REASON));
        assert_eq!(
            shown.action,
            [
                ActionBlock::Line(Phrase::with_path(
                    "write_file ",
                    PathText::from_raw(b"/ws/notes.md"),
                    ""
                )),
                ActionBlock::Note("Creates this file. Its content is not previewed here.")
            ]
        );
        let shown = ApprovalContent::from_request(
            &change("edit_file", "/etc/hosts", FileMutationState::Changes),
            Path::new("/ws"),
        );
        assert_eq!(shown.kind, "Edit file");
        assert_eq!(shown.question, "Would you like to edit this file?");
        assert_eq!(shown.reason.as_deref(), Some(EXTERNAL_CHANGE_REASON));
        assert_eq!(
            shown.action[1],
            ActionBlock::Note("Changes this file. The change is not previewed here.")
        );
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
    fn other_requests_show_their_arguments_preview_as_given() {
        let request = ApprovalRequest {
            id: RequestId::new(1),
            tool_name: "mcp_fixture_echo".to_owned(),
            title: "Calling mcp_fixture_echo".to_owned(),
            tool_arguments_preview: r#"{"text":"\x1b\x0a\xff sentinel"}"#.to_owned(),
            scope: ApprovalScope {
                target: None,
                access: PathAccess::WorkspaceOrExternal,
                always: None,
            },
            command: None,
            file: None,
        };
        let shown = ApprovalContent::from_request(&request, Path::new("/ws"));
        assert_eq!(
            shown.action,
            [ActionBlock::Arguments {
                target: "Calling mcp_fixture_echo".to_owned(),
                preview: r#"{"text":"\x1b\x0a\xff sentinel"}"#.to_owned(),
            }]
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
