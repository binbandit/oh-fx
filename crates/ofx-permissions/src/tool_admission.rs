use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ofx_contract::{
    Admission, ApplicableTarget, CommandRequest, FileMutation, FileMutationState, GatedAction,
    PathAccess, PermissionGate, PermissionMode, ToolCall,
};
use ofx_workspace::path_inside;

use crate::command_admission::{command_admission, undescribed_shell_call_admission};
use crate::permissions::{applicable_target, external_path_target};
use crate::session_permission_state::{EDIT_PERMISSION, SessionGrants, permission_name};

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

#[derive(Debug)]
pub struct PermissionPolicy {
    mode: PermissionMode,
    workspace_root: PathBuf,
    session_grants: SessionGrants,
}

impl PermissionPolicy {
    pub fn new(mode: PermissionMode, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            mode,
            workspace_root: workspace_root.into(),
            session_grants: SessionGrants::default(),
        }
    }
}

impl PermissionGate for PermissionPolicy {
    fn admit(&self, call: &ToolCall) -> Admission {
        if self.mode == PermissionMode::Yolo {
            return Admission::Allowed(PathAccess::WorkspaceOrExternal);
        }
        if let Some(admission) = undescribed_shell_call_admission(self.mode, call) {
            return admission;
        }
        match external_path_target(&self.workspace_root, call) {
            Some(target) if path_inside(&self.workspace_root, &target) => {
                Admission::Allowed(PathAccess::WorkspaceOnly)
            }
            Some(target)
                if self
                    .session_grants
                    .allow_path(permission_name(&call.name), &target) =>
            {
                Admission::Allowed(PathAccess::WorkspaceOrExternal)
            }
            Some(_) => Admission::ApprovalRequired,
            None => Admission::Allowed(PathAccess::WorkspaceOnly),
        }
    }

    fn admit_command(&self, request: &CommandRequest) -> Admission {
        if self.session_grants.allow_command(request) {
            return Admission::Allowed(PathAccess::WorkspaceOrExternal);
        }
        command_admission(self.mode, &self.workspace_root, request)
    }

    fn remember_approval(&self, action: GatedAction<'_>) {
        self.session_grants.remember(&self.workspace_root, action);
    }

    fn applicable_target(&self, call: &ToolCall) -> Option<ApplicableTarget> {
        applicable_target(&self.workspace_root, call)
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
        if self
            .session_grants
            .allow_path(EDIT_PERMISSION, &mutation.target)
        {
            return Admission::Allowed(access);
        }
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
    fn remembered_approvals_admit_later_reads_under_the_same_directory_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(root.join("notes/deep")).unwrap();
        fs::create_dir(root.join("other")).unwrap();
        for file in ["notes/a.txt", "notes/deep/b.txt", "other/c.txt"] {
            fs::write(root.join(file), "text\n").unwrap();
        }
        let paths = ["../notes/deep/b.txt", "../notes/a.txt", "../other/c.txt"];
        let admitted = |policy: &PermissionPolicy| -> Vec<Admission> {
            paths.iter().map(|path| policy.admit(&read(path))).collect()
        };
        let policy = PermissionPolicy::new(PermissionMode::Ask, &workspace);
        assert_eq!(admitted(&policy), [Admission::ApprovalRequired; 3]);
        policy.remember_approval(GatedAction::Call(&read("../notes/a.txt")));
        let external = Admission::Allowed(PathAccess::WorkspaceOrExternal);
        assert_eq!(
            admitted(&policy),
            [external, external, Admission::ApprovalRequired]
        );
        let search = ToolCall {
            name: "grep_files".to_owned(),
            arguments: r#"{"pattern":"x","path":"../notes"}"#.to_owned(),
            ..read("")
        };
        assert_eq!(policy.admit(&search), Admission::ApprovalRequired);

        let directory_grant = PermissionPolicy::new(PermissionMode::Auto, &workspace);
        directory_grant.remember_approval(GatedAction::Call(&read("../notes/deep")));
        assert_eq!(
            admitted(&directory_grant),
            [
                external,
                Admission::ApprovalRequired,
                Admission::ApprovalRequired
            ]
        );
    }

    #[test]
    fn approvals_for_tools_without_a_path_target_grant_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(root.join("outside.txt"), "text\n").unwrap();
        let policy = PermissionPolicy::new(PermissionMode::Ask, &workspace);
        policy.remember_approval(GatedAction::Call(&ToolCall {
            name: "unknown_tool".to_owned(),
            ..read("../outside.txt")
        }));
        policy.remember_approval(GatedAction::Command(&CommandRequest::Stop));
        assert_eq!(
            policy.admit(&read("../outside.txt")),
            Admission::ApprovalRequired
        );
        assert_eq!(
            policy.admit_command(&CommandRequest::Stop),
            Admission::ApprovalRequired
        );
    }

    #[test]
    fn remembered_file_changes_admit_later_changes_in_their_tree() {
        let policy = PermissionPolicy::new(PermissionMode::Ask, "/workspace");
        let inside = mutation("/workspace/src/main.rs", FileMutationState::Changes);
        let elsewhere = mutation("/elsewhere/notes/a.txt", FileMutationState::Changes);
        policy.remember_approval(GatedAction::FileMutation(&mutation(
            "/workspace/README.md",
            FileMutationState::Changes,
        )));
        assert_eq!(
            policy.admit_file_mutation(&inside),
            Admission::Allowed(PathAccess::WorkspaceOnly)
        );
        assert_eq!(
            policy.admit_file_mutation(&elsewhere),
            Admission::ApprovalRequired
        );
        policy.remember_approval(GatedAction::FileMutation(&mutation(
            "/elsewhere/notes/b.txt",
            FileMutationState::Creates,
        )));
        assert_eq!(
            policy.admit_file_mutation(&elsewhere),
            Admission::Allowed(PathAccess::WorkspaceOrExternal)
        );
        assert_eq!(
            policy.admit_file_mutation(&mutation(
                "/elsewhere/other.txt",
                FileMutationState::Changes
            )),
            Admission::ApprovalRequired
        );
    }

    #[test]
    fn remembered_commands_run_again_only_with_the_same_text() {
        let run = |command: &str| CommandRequest::Run {
            command: command.to_owned(),
            cwd: PathBuf::from("/workspace"),
            terminal: false,
        };
        let policy = PermissionPolicy::new(PermissionMode::Ask, "/workspace");
        policy.remember_approval(GatedAction::Command(&run("cargo test")));
        assert_eq!(
            policy.admit_command(&run("cargo test")),
            Admission::Allowed(PathAccess::WorkspaceOrExternal)
        );
        assert_eq!(
            policy.admit_command(&run("cargo test --release")),
            Admission::ApprovalRequired
        );
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
