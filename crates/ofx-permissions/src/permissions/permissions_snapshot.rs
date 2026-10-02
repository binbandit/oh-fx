use std::fmt::Write as _;
use std::path::{Component, Path};

use ofx_contract::{CommandProfile, PermissionMode, SessionGrant};
use ofx_text::{escape_terminal_controls, shell_word};

const WORKSPACE_FILE_PERMISSIONS: [&str; 4] = ["edit", "read", "glob", "grep"];

pub(crate) fn interactive_body(
    workspace_root: &Path,
    mode: PermissionMode,
    grants: &[SessionGrant],
) -> String {
    let mut body = format!("mode={}\nconfigured rules: (none)\n", mode.display_label());
    let lines: Vec<String> = grants
        .iter()
        .flat_map(|grant| grant_lines(workspace_root, grant))
        .collect();
    if lines.is_empty() {
        body.push_str("session grants: (none)");
        return body;
    }
    body.push_str("session grants:\n");
    body.push_str(&lines.join("\n"));
    body
}

fn grant_lines(workspace_root: &Path, grant: &SessionGrant) -> Vec<String> {
    let tree = |permission: &str, root: &Path| {
        format!(" - {permission} -> {}", tree_pattern(workspace_root, root))
    };
    match grant {
        SessionGrant::WorkspaceFiles => WORKSPACE_FILE_PERMISSIONS
            .iter()
            .map(|permission| tree(permission, workspace_root))
            .collect(),
        SessionGrant::FileChangesUnder(root) => vec![tree("edit", root)],
        SessionGrant::ReadsUnder(root) => vec![tree("read", root)],
        SessionGrant::GlobsUnder(root) => vec![tree("glob", root)],
        SessionGrant::GrepsUnder(root) => vec![tree("grep", root)],
        SessionGrant::Command {
            command,
            cwd,
            profile,
            shell,
            terminal,
        } => {
            let mut line = format!(
                " - bash -> {} (cwd={}",
                escape_terminal_controls(command),
                displayed(cwd)
            );
            if *profile == CommandProfile::Clean {
                line.push_str(", profile=clean");
            }
            if *terminal {
                line.push_str(", tty=true");
            }
            if let Some(shell) = shell {
                let _ = write!(line, ", shell={}", displayed(shell));
            }
            line.push(')');
            vec![line]
        }
    }
}

fn displayed(path: &Path) -> String {
    shell_word(&escape_terminal_controls(&path.to_string_lossy())).into_owned()
}

fn tree_pattern(workspace_root: &Path, root: &Path) -> String {
    let relative = escape_terminal_controls(&relative_path(workspace_root, root)).into_owned();
    if relative.is_empty() {
        "**".to_owned()
    } else {
        format!("{relative}/**")
    }
}

fn relative_path(from: &Path, to: &Path) -> String {
    let named = |path: &Path| -> Vec<String> {
        path.components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect()
    };
    let from = named(from);
    let to = named(to);
    let shared = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut parts = vec!["..".to_owned(); from.len() - shared];
    parts.extend_from_slice(&to[shared..]);
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn the_interactive_body_names_the_mode_and_reports_no_rules_or_grants() {
        assert_eq!(
            interactive_body(Path::new("/ws"), PermissionMode::Auto, &[]),
            "mode=auto\nconfigured rules: (none)\nsession grants: (none)"
        );
        assert_eq!(
            interactive_body(Path::new("/ws"), PermissionMode::Yolo, &[]),
            "mode=full access\nconfigured rules: (none)\nsession grants: (none)"
        );
    }

    #[test]
    fn session_grants_are_listed_as_upstream_permission_patterns_relative_to_the_workspace() {
        let grants = [
            SessionGrant::WorkspaceFiles,
            SessionGrant::ReadsUnder(PathBuf::from("/home/me/notes")),
            SessionGrant::FileChangesUnder(PathBuf::from("/")),
            SessionGrant::GlobsUnder(PathBuf::from("/home/me/ws/src")),
            SessionGrant::GrepsUnder(PathBuf::from("/home/me")),
            SessionGrant::Command {
                command: "git status".to_owned(),
                cwd: PathBuf::from("/home/me/ws"),
                profile: CommandProfile::User,
                shell: None,
                terminal: false,
            },
            SessionGrant::Command {
                command: "npm test".to_owned(),
                cwd: PathBuf::from("/home/me/ws/app"),
                profile: CommandProfile::Clean,
                shell: Some(PathBuf::from("/bin/sh")),
                terminal: true,
            },
        ];
        assert_eq!(
            interactive_body(Path::new("/home/me/ws"), PermissionMode::Ask, &grants),
            concat!(
                "mode=ask\nconfigured rules: (none)\nsession grants:\n",
                " - edit -> **\n - read -> **\n - glob -> **\n - grep -> **\n",
                " - read -> ../notes/**\n",
                " - edit -> ../../../**\n",
                " - glob -> src/**\n",
                " - grep -> ../**\n",
                " - bash -> git status (cwd=/home/me/ws)\n",
                " - bash -> npm test (cwd=/home/me/ws/app, profile=clean, tty=true, shell=/bin/sh)",
            )
        );
    }

    #[test]
    fn line_breaks_and_controls_in_a_grant_cannot_forge_another_grant_line() {
        let grants = [
            SessionGrant::ReadsUnder(PathBuf::from("/ws/notes\n - edit -> **")),
            SessionGrant::Command {
                command: "true\n - read -> **\x1b[2J".to_owned(),
                cwd: PathBuf::from("/ws/a\rb"),
                profile: CommandProfile::User,
                shell: Some(PathBuf::from("/bin/\x07sh")),
                terminal: true,
            },
        ];
        assert_eq!(
            interactive_body(Path::new("/ws"), PermissionMode::Auto, &grants),
            concat!(
                "mode=auto\nconfigured rules: (none)\nsession grants:\n",
                " - read -> notes\\x0a - edit -> **/**\n",
                " - bash -> true\\x0a - read -> **\\x1b[2J (cwd='/ws/a\\x0db', tty=true, shell='/bin/\\x07sh')",
            )
        );
    }

    #[test]
    fn a_directory_or_shell_name_cannot_pass_for_another_setting_of_the_grant() {
        let command = |cwd: &str, shell: Option<&str>| SessionGrant::Command {
            command: "make".to_owned(),
            cwd: PathBuf::from(cwd),
            profile: CommandProfile::User,
            shell: shell.map(PathBuf::from),
            terminal: false,
        };
        let grants = [
            command("/ws/x, profile=clean", Some("/bin/sh, tty=true")),
            command("/ws/it's here", None),
            command("/ws/v1.2_a-b+c,d:e@f%g", Some("/usr/bin/zsh")),
        ];
        assert_eq!(
            interactive_body(Path::new("/ws"), PermissionMode::Ask, &grants),
            concat!(
                "mode=ask\nconfigured rules: (none)\nsession grants:\n",
                " - bash -> make (cwd='/ws/x, profile=clean', shell='/bin/sh, tty=true')\n",
                " - bash -> make (cwd='/ws/it'\\''s here')\n",
                " - bash -> make (cwd=/ws/v1.2_a-b+c,d:e@f%g, shell=/usr/bin/zsh)",
            )
        );
    }
}
