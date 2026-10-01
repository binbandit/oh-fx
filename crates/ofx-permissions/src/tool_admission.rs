use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ofx_contract::{
    Admission, FileMutation, FileMutationState, PathAccess, PermissionGate, PermissionMode,
    ToolCall,
};
use ofx_workspace::path_inside;

use crate::permissions::external_path_target;

const SENSITIVE_AUTO_WRITE_TARGETS: [&[&str]; 26] = [
    &[".git"],
    &[".ssh", "authorized_keys"],
    &[".ssh", "config"],
    &[".ssh", "rc"],
    &["library", "launchagents"],
    &["library", "launchdaemons"],
    &[".config", "autostart"],
    &[".config", "environment.d"],
    &[".config", "systemd", "user"],
    &[".local", "share", "systemd", "user"],
    &[".config", "fish", "config.fish"],
    &[".config", "fish", "conf.d"],
    &[".config", "git", "config"],
    &[".gitconfig"],
    &[".zshrc"],
    &[".zshenv"],
    &[".zprofile"],
    &[".zlogin"],
    &[".zlogout"],
    &[".bashrc"],
    &[".bash_profile"],
    &[".bash_login"],
    &[".bash_logout"],
    &[".bash_aliases"],
    &[".profile"],
    &[".pam_environment"],
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPolicy {
    mode: PermissionMode,
    workspace_root: PathBuf,
}

impl PermissionPolicy {
    pub fn new(mode: PermissionMode, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            mode,
            workspace_root: workspace_root.into(),
        }
    }
}

impl PermissionGate for PermissionPolicy {
    fn admit(&self, call: &ToolCall) -> Admission {
        if self.mode == PermissionMode::Yolo {
            return Admission::Allowed(PathAccess::WorkspaceOrExternal);
        }
        match external_path_target(&self.workspace_root, call) {
            Some(target) if !path_inside(&self.workspace_root, &target) => {
                Admission::ApprovalRequired
            }
            _ => Admission::Allowed(PathAccess::WorkspaceOnly),
        }
    }

    fn admit_file_mutation(&self, mutation: &FileMutation) -> Admission {
        if self.mode == PermissionMode::Yolo {
            return Admission::Allowed(PathAccess::WorkspaceOrExternal);
        }
        let inside = path_inside(&self.workspace_root, &mutation.target);
        let access = if inside {
            PathAccess::WorkspaceOnly
        } else {
            PathAccess::WorkspaceOrExternal
        };
        let reversible = inside || mutation.state == FileMutationState::Creates;
        match mutation.state {
            FileMutationState::Unchanged if inside => Admission::Allowed(access),
            _ if self.mode == PermissionMode::Ask => Admission::ApprovalRequired,
            FileMutationState::Creates | FileMutationState::Changes
                if reversible && !sensitive_auto_write_target(&mutation.target) =>
            {
                Admission::Allowed(access)
            }
            _ => Admission::ReviewUnavailable,
        }
    }
}

fn sensitive_auto_write_target(path: &Path) -> bool {
    SENSITIVE_AUTO_WRITE_TARGETS
        .iter()
        .any(|sequence| path_contains_component_sequence(path, sequence))
}

fn path_contains_component_sequence(path: &Path, expected: &[&str]) -> bool {
    let Some(first) = expected.first() else {
        return false;
    };
    let mut matched = 0;
    for component in path
        .as_os_str()
        .as_bytes()
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty())
        .map(folded)
    {
        if component == expected[matched] {
            matched += 1;
            if matched == expected.len() {
                return true;
            }
        } else {
            matched = usize::from(component == *first);
        }
    }
    false
}

fn folded(component: &[u8]) -> String {
    let once = case_mapped(String::from_utf8_lossy(component).chars());
    case_mapped(once.chars())
}

fn case_mapped(characters: impl Iterator<Item = char>) -> String {
    characters
        .filter(|character| !is_ignorable_in_names(*character))
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_ignorable_in_names(character: char) -> bool {
    matches!(
        character,
        '\u{ad}'
            | '\u{34f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    use ofx_contract::ToolCallId;

    use super::*;

    fn read(path: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("call-1"),
            name: "read_file".to_owned(),
            arguments: format!(r#"{{"path":"{path}"}}"#),
        }
    }

    fn admissions(mode: PermissionMode, workspace: &Path, paths: &[&str]) -> Vec<Admission> {
        let policy = PermissionPolicy::new(mode, workspace);
        paths.iter().map(|path| policy.admit(&read(path))).collect()
    }

    #[test]
    fn reads_outside_the_workspace_need_approval_unless_full_access_is_on() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::create_dir(root.join("outside")).unwrap();
        fs::write(root.join("outside/secret.txt"), "secret\n").unwrap();
        symlink(root.join("outside"), workspace.join("link")).unwrap();
        let external = root.join("outside/secret.txt");
        let external = external.to_str().unwrap();
        let reentry = workspace.join("src/main.rs");
        let paths = [
            "src/main.rs",
            reentry.to_str().unwrap(),
            "../workspace/src/main.rs",
            "missing.txt",
            "link/secret.txt",
            external,
            "../outside/secret.txt",
        ];
        let workspace_only = Admission::Allowed(PathAccess::WorkspaceOnly);

        for mode in [PermissionMode::Ask, PermissionMode::Auto] {
            assert_eq!(
                admissions(mode, &workspace, &paths),
                [
                    workspace_only,
                    workspace_only,
                    workspace_only,
                    workspace_only,
                    workspace_only,
                    Admission::ApprovalRequired,
                    Admission::ApprovalRequired,
                ],
                "{mode:?}"
            );
        }
        assert_eq!(
            admissions(PermissionMode::Yolo, &workspace, &paths),
            [Admission::Allowed(PathAccess::WorkspaceOrExternal); 7]
        );
    }

    #[test]
    fn searches_outside_the_workspace_need_approval_unless_full_access_is_on() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(workspace.join("src")).unwrap();
        let search = |name: &str, path: Option<&str>| ToolCall {
            id: ToolCallId::new("call-1"),
            name: name.to_owned(),
            arguments: match path {
                Some(path) => format!(r#"{{"pattern":"x","path":"{path}"}}"#),
                None => r#"{"pattern":"x"}"#.to_owned(),
            },
        };
        let outside = root.to_str().unwrap();

        for name in ["glob_files", "grep_files"] {
            for mode in [PermissionMode::Ask, PermissionMode::Auto] {
                let policy = PermissionPolicy::new(mode, &workspace);
                for path in [None, Some("."), Some("src")] {
                    assert_eq!(
                        policy.admit(&search(name, path)),
                        Admission::Allowed(PathAccess::WorkspaceOnly),
                        "{name} {mode:?} {path:?}"
                    );
                }
                for path in ["..", outside] {
                    assert_eq!(
                        policy.admit(&search(name, Some(path))),
                        Admission::ApprovalRequired,
                        "{name} {mode:?} {path}"
                    );
                }
            }
            assert_eq!(
                PermissionPolicy::new(PermissionMode::Yolo, &workspace)
                    .admit(&search(name, Some(outside))),
                Admission::Allowed(PathAccess::WorkspaceOrExternal)
            );
        }
    }

    fn mutation(target: &str, state: FileMutationState) -> FileMutation {
        FileMutation {
            target: PathBuf::from(target),
            state,
        }
    }

    #[test]
    fn file_mutations_follow_upstream_admission_for_each_permission_mode() {
        use FileMutationState::{Changes, Creates, Unchanged, Unread};

        let inside = Admission::Allowed(PathAccess::WorkspaceOnly);
        let outside = Admission::Allowed(PathAccess::WorkspaceOrExternal);
        let held = Admission::ReviewUnavailable;
        let cases = [
            ("/workspace/src/main.rs", Changes, inside),
            ("/workspace/new/file.rs", Creates, inside),
            ("/workspace/src/main.rs", Unchanged, inside),
            ("/elsewhere/new.txt", Creates, outside),
            ("/elsewhere/notes.txt", Unread, held),
            ("/elsewhere/notes.txt", Unchanged, held),
            ("/workspace/.git/hooks/pre-commit", Creates, held),
            ("/workspace/.git/config", Changes, held),
            ("/home/user/.bashrc", Creates, held),
            ("/home/user/.zshenv", Creates, held),
            ("/home/user/.ssh/rc", Creates, held),
            ("/home/user/.config/systemd/user/a.service", Creates, held),
        ];
        let policy = |mode| PermissionPolicy::new(mode, "/workspace");
        for (target, state, expected) in cases {
            let mutation = mutation(target, state);
            assert_eq!(
                policy(PermissionMode::Auto).admit_file_mutation(&mutation),
                expected,
                "auto {target} {state:?}"
            );
            let in_ask = if state == Unchanged && target.starts_with("/workspace/") {
                inside
            } else {
                Admission::ApprovalRequired
            };
            assert_eq!(
                policy(PermissionMode::Ask).admit_file_mutation(&mutation),
                in_ask,
                "ask {target} {state:?}"
            );
            assert_eq!(
                policy(PermissionMode::Yolo).admit_file_mutation(&mutation),
                outside,
                "yolo {target} {state:?}"
            );
        }
    }

    #[test]
    fn sensitive_auto_write_targets_match_whole_components_in_any_letter_case() {
        for target in [
            "/workspace/.GIT/Hooks/pre-commit",
            "/workspace/.git/hooks",
            "/workspace/.git/modules/sub/hooks/post-checkout",
            "/workspace/.git/worktrees/tree/config.worktree",
            "/workspace/.git/info/exclude",
            "/Users/me/library/launchagents/x.plist",
            "/home/user/.BashRC",
            "/home/user/.bash_aliases",
            "/home/user/.config/fish/conf.d/x.fish",
            "/home/user/.local/share/systemd/user/x.service",
        ] {
            assert!(sensitive_auto_write_target(Path::new(target)), "{target}");
        }
        for target in [
            "/workspace/.github/hooks/x",
            "/workspace/git/hooks/x",
            "/workspace/.gitignore",
            "/workspace/.gitconfig.d/x",
            "/workspace/notes/.profile.bak",
        ] {
            assert!(!sensitive_auto_write_target(Path::new(target)), "{target}");
        }
        assert!(!path_contains_component_sequence(Path::new("/a"), &[]));
    }

    #[test]
    fn sensitive_auto_write_targets_match_names_a_case_insensitive_filesystem_folds_together() {
        for target in [
            "/Users/me/.\u{17f}sh/rc",
            "/Users/me/.ssh/authorized_\u{212a}eys",
            "/workspace/.GIT/hooks/pre-commit",
            "/Users/me/.z\u{17f}hrc",
            "/Users/me/.bash\u{200d}rc",
            "/Users/me/.bash_pro\u{fb01}le",
            "/Users/me/.\u{1e9e}h/rc",
        ] {
            assert!(sensitive_auto_write_target(Path::new(target)), "{target:?}");
        }
    }

    #[test]
    fn sensitive_auto_write_targets_are_listed_in_folded_form() {
        for sequence in SENSITIVE_AUTO_WRITE_TARGETS {
            for name in sequence {
                assert_eq!(folded(name.as_bytes()), *name);
            }
        }
    }

    #[test]
    fn calls_without_an_external_path_target_stay_inside_the_workspace() {
        let policy = PermissionPolicy::new(PermissionMode::Ask, "/");
        let call = ToolCall {
            id: ToolCallId::new("call-1"),
            name: "unknown_tool".to_owned(),
            arguments: r#"{"path":"/etc/hosts"}"#.to_owned(),
        };
        assert_eq!(
            policy.admit(&call),
            Admission::Allowed(PathAccess::WorkspaceOnly)
        );
    }
}
