use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ofx_contract::{
    Admission, ApplicableTarget, ApprovalScope, CommandRequest, FileMutation, FileMutationState,
    GatedAction, LivePermissionMode, PathAccess, PermissionGate, PermissionMode, SessionGrant,
    ToolCall,
};
use ofx_workspace::path_inside;

use crate::command_admission::{command_admission, undescribed_shell_call_admission};
use crate::permissions::{applicable_target, external_path_target, interactive_body};
use crate::session_permission_state::{SessionGrants, TreePermission, command_grant};

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
    mode: LivePermissionMode,
    workspace_root: PathBuf,
    session_grants: SessionGrants,
}

impl PermissionPolicy {
    pub fn new(mode: impl Into<LivePermissionMode>, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            mode: mode.into(),
            workspace_root: workspace_root.into(),
            session_grants: SessionGrants::default(),
        }
    }

    pub fn notice_body(&self) -> String {
        interactive_body(
            &self.workspace_root,
            self.mode.get(),
            &self.session_grants.snapshot(),
        )
    }
}

impl PermissionGate for PermissionPolicy {
    fn admit(&self, call: &ToolCall) -> Admission {
        let mode = self.mode.get();
        if mode == PermissionMode::Yolo {
            return Admission::Allowed(PathAccess::WorkspaceOrExternal);
        }
        if let Some(admission) = undescribed_shell_call_admission(mode, call) {
            return admission;
        }
        match external_path_target(&self.workspace_root, call) {
            Some(target) if path_inside(&self.workspace_root, &target) => {
                Admission::Allowed(PathAccess::WorkspaceOnly)
            }
            Some(target) => TreePermission::of_tool(&call.name)
                .and_then(|permission| {
                    self.session_grants
                        .granted_root(&self.workspace_root, permission, &target)
                })
                .map_or(Admission::ApprovalRequired, |root| {
                    Admission::Allowed(PathAccess::Within(root))
                }),
            None => Admission::Allowed(PathAccess::WorkspaceOnly),
        }
    }

    fn admit_command(&self, request: &CommandRequest) -> Admission {
        if self.session_grants.allow_command(request) {
            return Admission::Allowed(PathAccess::WorkspaceOrExternal);
        }
        command_admission(self.mode.get(), &self.workspace_root, request)
    }

    fn approval_scope(&self, action: GatedAction<'_>) -> ApprovalScope {
        match action {
            GatedAction::Call(call) => self.call_approval_scope(call),
            GatedAction::FileMutation(mutation) => ApprovalScope {
                target: None,
                access: PathAccess::WorkspaceOrExternal,
                always: self.file_change_grant(&mutation.target),
            },
            GatedAction::Command(request) => ApprovalScope {
                target: None,
                access: PathAccess::WorkspaceOrExternal,
                always: command_grant(request).filter(|_| runs_in_an_existing_directory(request)),
            },
        }
    }

    fn remember_approval(&self, grant: &SessionGrant) {
        self.session_grants.remember(grant);
    }

    fn forget_approvals(&self) {
        self.session_grants.forget();
    }

    fn applicable_target(&self, call: &ToolCall) -> Option<ApplicableTarget> {
        applicable_target(&self.workspace_root, call)
    }

    fn admit_file_mutation(&self, mutation: &FileMutation) -> Admission {
        let mode = self.mode.get();
        if mode == PermissionMode::Yolo {
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
            .granted_root(&self.workspace_root, TreePermission::Edit, &mutation.target)
            .is_some()
        {
            return Admission::Allowed(access);
        }
        match mutation.state {
            FileMutationState::Unchanged if inside => Admission::Allowed(access),
            _ if mode == PermissionMode::Ask => Admission::ApprovalRequired,
            FileMutationState::Creates | FileMutationState::Changes
                if reversible && !sensitive_auto_write_target(&mutation.target) =>
            {
                Admission::Allowed(access)
            }
            _ => Admission::ReviewUnavailable,
        }
    }
}

impl PermissionPolicy {
    fn call_approval_scope(&self, call: &ToolCall) -> ApprovalScope {
        let target = external_path_target(&self.workspace_root, call);
        let tree = target
            .as_deref()
            .filter(|target| !path_inside(&self.workspace_root, target))
            .and_then(external_grant_root)
            .map(Path::to_path_buf);
        ApprovalScope {
            always: tree
                .clone()
                .zip(TreePermission::of_tool(&call.name))
                .map(|(tree, permission)| permission.grant_under(tree)),
            access: tree.map_or(PathAccess::WorkspaceOnly, PathAccess::Within),
            target,
        }
    }

    fn file_change_grant(&self, target: &Path) -> Option<SessionGrant> {
        if path_inside(&self.workspace_root, target) {
            return Some(SessionGrant::WorkspaceFiles);
        }
        target
            .ancestors()
            .skip(1)
            .find(|ancestor| ancestor.is_dir())
            .map(|root| TreePermission::Edit.grant_under(root.to_path_buf()))
    }
}

fn runs_in_an_existing_directory(request: &CommandRequest) -> bool {
    matches!(request, CommandRequest::Run { cwd, .. } if cwd.is_absolute() && cwd.is_dir())
}

fn external_grant_root(target: &Path) -> Option<&Path> {
    if target.is_dir() {
        Some(target)
    } else {
        target.parent()
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

    use ofx_contract::{CommandProfile, ToolCallId};

    use super::*;

    fn read(path: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("call-1"),
            name: "read_file".to_owned(),
            arguments: format!(r#"{{"path":"{path}"}}"#),
        }
    }

    fn approve_always(policy: &PermissionPolicy, action: GatedAction<'_>) {
        if let Some(grant) = policy.approval_scope(action).always {
            policy.remember_approval(&grant);
        }
    }

    fn admissions(mode: PermissionMode, workspace: &Path, paths: &[&str]) -> Vec<Admission> {
        let policy = PermissionPolicy::new(mode, workspace);
        paths.iter().map(|path| policy.admit(&read(path))).collect()
    }

    #[test]
    fn reads_outside_the_workspace_need_approval_unless_full_access_is_on() {
        const WORKSPACE_ONLY: Admission = Admission::Allowed(PathAccess::WorkspaceOnly);

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

        for mode in [PermissionMode::Ask, PermissionMode::Auto] {
            assert_eq!(
                admissions(mode, &workspace, &paths),
                [
                    WORKSPACE_ONLY,
                    WORKSPACE_ONLY,
                    WORKSPACE_ONLY,
                    WORKSPACE_ONLY,
                    WORKSPACE_ONLY,
                    Admission::ApprovalRequired,
                    Admission::ApprovalRequired,
                ],
                "{mode:?}"
            );
        }
        assert_eq!(
            admissions(PermissionMode::Yolo, &workspace, &paths),
            [const { Admission::Allowed(PathAccess::WorkspaceOrExternal) }; 7]
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

    #[test]
    fn a_mode_switched_while_the_policy_is_shared_decides_the_next_admission() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(root.join("notes.txt"), "notes\n").unwrap();
        let live = LivePermissionMode::from(PermissionMode::Ask);
        let policy = PermissionPolicy::new(live.clone(), &workspace);
        let outside = read("../notes.txt");
        let change = mutation("/elsewhere/notes.txt", FileMutationState::Unread);
        let status = CommandRequest::Run {
            command: "git status".to_owned(),
            cwd: workspace.clone(),
            profile: CommandProfile::User,
            shell: None,
            terminal: false,
        };
        assert_eq!(policy.admit(&outside), Admission::ApprovalRequired);
        assert_eq!(policy.admit_command(&status), Admission::ApprovalRequired);
        live.set(PermissionMode::Yolo);
        assert_eq!(
            policy.admit(&outside),
            Admission::Allowed(PathAccess::WorkspaceOrExternal)
        );
        assert_eq!(
            policy.admit_file_mutation(&change),
            Admission::Allowed(PathAccess::WorkspaceOrExternal)
        );
        live.set(PermissionMode::Auto);
        assert_eq!(
            policy.admit_command(&status),
            Admission::Allowed(PathAccess::WorkspaceOnly)
        );
        assert_eq!(
            policy.admit_file_mutation(&change),
            Admission::ReviewUnavailable
        );
        live.set(PermissionMode::Ask);
        assert_eq!(policy.admit(&outside), Admission::ApprovalRequired);
    }

    #[test]
    fn the_notice_body_follows_the_live_mode_and_lists_grants_once_until_forgotten() {
        let live = LivePermissionMode::from(PermissionMode::Ask);
        let policy = PermissionPolicy::new(live.clone(), "/workspace");
        let empty = "configured rules: (none)\nsession grants: (none)";
        assert_eq!(policy.notice_body(), format!("mode=ask\n{empty}"));
        let reads = SessionGrant::ReadsUnder(PathBuf::from("/elsewhere"));
        for grant in [
            &reads,
            &SessionGrant::GrepsUnder(PathBuf::from("/elsewhere")),
            &reads,
        ] {
            policy.remember_approval(grant);
        }
        live.set(PermissionMode::Yolo);
        assert_eq!(
            policy.notice_body(),
            "mode=full access\nconfigured rules: (none)\nsession grants:\n - read -> ../elsewhere/**\n - grep -> ../elsewhere/**"
        );
        policy.forget_approvals();
        assert_eq!(policy.notice_body(), format!("mode=full access\n{empty}"));
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

        const INSIDE: Admission = Admission::Allowed(PathAccess::WorkspaceOnly);
        const OUTSIDE: Admission = Admission::Allowed(PathAccess::WorkspaceOrExternal);
        const HELD: Admission = Admission::ReviewUnavailable;
        let cases = [
            ("/workspace/src/main.rs", Changes, INSIDE),
            ("/workspace/new/file.rs", Creates, INSIDE),
            ("/workspace/src/main.rs", Unchanged, INSIDE),
            ("/elsewhere/new.txt", Creates, OUTSIDE),
            ("/elsewhere/notes.txt", Unread, HELD),
            ("/elsewhere/notes.txt", Unchanged, HELD),
            ("/workspace/.git/hooks/pre-commit", Creates, HELD),
            ("/workspace/.git/config", Changes, HELD),
            ("/home/user/.bashrc", Creates, HELD),
            ("/home/user/.zshenv", Creates, HELD),
            ("/home/user/.ssh/rc", Creates, HELD),
            ("/home/user/.config/systemd/user/a.service", Creates, HELD),
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
                INSIDE
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
                OUTSIDE,
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
        assert_eq!(
            admitted(&policy),
            [const { Admission::ApprovalRequired }; 3]
        );
        approve_always(&policy, GatedAction::Call(&read("../notes/a.txt")));
        let within = |tree: &str| Admission::Allowed(PathAccess::Within(root.join(tree)));
        assert_eq!(
            admitted(&policy),
            [
                within("notes"),
                within("notes"),
                Admission::ApprovalRequired
            ]
        );
        let search = ToolCall {
            name: "grep_files".to_owned(),
            arguments: r#"{"pattern":"x","path":"../notes"}"#.to_owned(),
            ..read("")
        };
        assert_eq!(policy.admit(&search), Admission::ApprovalRequired);

        let directory_grant = PermissionPolicy::new(PermissionMode::Auto, &workspace);
        approve_always(&directory_grant, GatedAction::Call(&read("../notes/deep")));
        assert_eq!(
            admitted(&directory_grant),
            [
                within("notes/deep"),
                Admission::ApprovalRequired,
                Admission::ApprovalRequired
            ]
        );
        approve_always(&directory_grant, GatedAction::Call(&read("../notes/a.txt")));
        assert_eq!(
            admitted(&directory_grant),
            [
                within("notes"),
                within("notes"),
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
        approve_always(
            &policy,
            GatedAction::Call(&ToolCall {
                name: "unknown_tool".to_owned(),
                ..read("../outside.txt")
            }),
        );
        approve_always(&policy, GatedAction::Command(&CommandRequest::Stop));
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
    fn approved_calls_run_confined_to_the_tree_their_approval_covers() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::create_dir_all(root.join("notes/deep")).unwrap();
        fs::write(root.join("notes/a.txt"), "text\n").unwrap();
        let policy = PermissionPolicy::new(PermissionMode::Ask, &workspace);
        let scope = |call: &ToolCall| policy.approval_scope(GatedAction::Call(call));
        let within = |target: &str, tree: &str, grant: fn(PathBuf) -> SessionGrant| ApprovalScope {
            target: Some(root.join(target)),
            access: PathAccess::Within(root.join(tree)),
            always: Some(grant(root.join(tree))),
        };
        assert_eq!(
            scope(&read("../notes/a.txt")),
            within("notes/a.txt", "notes", SessionGrant::ReadsUnder)
        );
        assert_eq!(
            scope(&read("../notes/deep")),
            within("notes/deep", "notes/deep", SessionGrant::ReadsUnder)
        );
        let search = |name: &str, path: &str| ToolCall {
            name: name.to_owned(),
            arguments: format!(r#"{{"pattern":"x","path":"{path}"}}"#),
            ..read("")
        };
        assert_eq!(
            scope(&search("grep_files", "../notes/deep")),
            within("notes/deep", "notes/deep", SessionGrant::GrepsUnder)
        );
        assert_eq!(
            scope(&search("glob_files", "../notes")),
            within("notes", "notes", SessionGrant::GlobsUnder)
        );
        assert_eq!(
            scope(&search("glob_files", "src")),
            ApprovalScope {
                target: Some(workspace.join("src")),
                access: PathAccess::WorkspaceOnly,
                always: None,
            }
        );
        for vanished in [read("../notes/gone.txt"), search("grep_files", "../gone")] {
            assert_eq!(
                scope(&vanished),
                ApprovalScope {
                    target: None,
                    access: PathAccess::WorkspaceOnly,
                    always: None,
                }
            );
        }
        assert_eq!(
            policy.approval_scope(GatedAction::Command(&CommandRequest::Run {
                command: "cargo test".to_owned(),
                cwd: workspace.clone(),
                profile: CommandProfile::Clean,
                shell: None,
                terminal: true,
            })),
            ApprovalScope {
                target: None,
                access: PathAccess::WorkspaceOrExternal,
                always: Some(SessionGrant::Command {
                    command: "cargo test".to_owned(),
                    cwd: workspace.clone(),
                    profile: CommandProfile::Clean,
                    shell: None,
                    terminal: true,
                }),
            }
        );
    }

    #[test]
    fn file_changes_offer_the_workspace_or_the_nearest_existing_tree_outside_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(workspace.join(".git/hooks")).unwrap();
        fs::create_dir_all(root.join("notes")).unwrap();
        fs::write(root.join("notes/a.txt"), "text\n").unwrap();
        let policy = PermissionPolicy::new(PermissionMode::Ask, &workspace);
        let offered = |target: PathBuf| {
            let scope = policy.approval_scope(GatedAction::FileMutation(&FileMutation {
                target,
                state: FileMutationState::Changes,
            }));
            assert_eq!(
                (scope.target, scope.access),
                (None, PathAccess::WorkspaceOrExternal)
            );
            scope.always
        };
        for inside in ["src/new.rs", ".git/hooks/pre-commit"] {
            assert_eq!(
                offered(workspace.join(inside)),
                Some(SessionGrant::WorkspaceFiles),
                "{inside}"
            );
        }
        let under = |tree: &Path| Some(SessionGrant::FileChangesUnder(tree.to_path_buf()));
        for (target, tree) in [
            ("notes/a.txt", "notes"),
            ("notes/new.txt", "notes"),
            ("notes/a/b/new.txt", "notes"),
            ("missing/new.txt", ""),
        ] {
            assert_eq!(
                offered(root.join(target)),
                under(&root.join(tree).components().collect::<PathBuf>()),
                "{target}"
            );
        }
        assert_eq!(offered(PathBuf::from("/new.txt")), under(Path::new("/")));
        assert_eq!(offered(PathBuf::from("/")), None);
    }

    #[test]
    fn always_remembers_the_tree_captured_before_the_prompt_even_if_the_target_moves() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        for file in ["approved/a.txt", "unapproved/a.txt", "unapproved/b.txt"] {
            fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
            fs::write(root.join(file), "text\n").unwrap();
        }
        symlink(root.join("approved"), root.join("link")).unwrap();
        let policy = PermissionPolicy::new(PermissionMode::Ask, &workspace);
        let call = read("../link/a.txt");
        let scope = policy.approval_scope(GatedAction::Call(&call));
        fs::remove_file(root.join("link")).unwrap();
        symlink(root.join("unapproved"), root.join("link")).unwrap();
        policy.remember_approval(&scope.always.unwrap());
        assert_eq!(
            policy.admit(&read("../unapproved/b.txt")),
            Admission::ApprovalRequired
        );
        assert_eq!(policy.admit(&call), Admission::ApprovalRequired);
        assert_eq!(
            policy.admit(&read("../approved/a.txt")),
            Admission::Allowed(PathAccess::Within(root.join("approved")))
        );
    }

    #[test]
    fn remembered_file_changes_admit_later_changes_in_their_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(root.join("elsewhere/notes")).unwrap();
        fs::write(root.join("elsewhere/notes/a.txt"), "text\n").unwrap();
        let change = |target: PathBuf, state| FileMutation { target, state };
        let policy = PermissionPolicy::new(PermissionMode::Ask, &workspace);
        let inside = change(workspace.join("src/main.rs"), FileMutationState::Changes);
        let elsewhere = change(
            root.join("elsewhere/notes/a.txt"),
            FileMutationState::Changes,
        );
        approve_always(
            &policy,
            GatedAction::FileMutation(&change(
                workspace.join("README.md"),
                FileMutationState::Changes,
            )),
        );
        assert_eq!(
            policy.admit_file_mutation(&inside),
            Admission::Allowed(PathAccess::WorkspaceOnly)
        );
        assert_eq!(
            policy.admit_file_mutation(&elsewhere),
            Admission::ApprovalRequired
        );
        approve_always(
            &policy,
            GatedAction::FileMutation(&change(
                root.join("elsewhere/notes/b.txt"),
                FileMutationState::Creates,
            )),
        );
        assert_eq!(
            policy.admit_file_mutation(&elsewhere),
            Admission::Allowed(PathAccess::WorkspaceOrExternal)
        );
        assert_eq!(
            policy.admit_file_mutation(&change(
                root.join("elsewhere/other.txt"),
                FileMutationState::Changes
            )),
            Admission::ApprovalRequired
        );
        assert_eq!(
            policy.admit(&read("../elsewhere/notes/a.txt")),
            Admission::ApprovalRequired
        );
    }

    #[test]
    fn remembered_commands_run_again_only_with_the_same_text_directory_profile_and_terminal_mode() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fs::canonicalize(temp.path()).unwrap();
        let run = |command: &str, cwd: PathBuf, profile, terminal| CommandRequest::Run {
            command: command.to_owned(),
            cwd,
            profile,
            shell: None,
            terminal,
        };
        let cargo_test = |cwd: PathBuf| run("cargo test", cwd, CommandProfile::Clean, false);
        let policy = PermissionPolicy::new(PermissionMode::Ask, &workspace);
        for unresolved in [workspace.join("missing"), PathBuf::from("../missing")] {
            let scope = policy.approval_scope(GatedAction::Command(&cargo_test(unresolved)));
            assert_eq!(scope.always, None);
        }
        approve_always(
            &policy,
            GatedAction::Command(&cargo_test(workspace.clone())),
        );
        assert_eq!(
            policy.admit_command(&cargo_test(workspace.clone())),
            Admission::Allowed(PathAccess::WorkspaceOrExternal)
        );
        for different in [
            cargo_test(workspace.join("sub")),
            cargo_test(PathBuf::from("/root")),
            cargo_test(PathBuf::from("/")),
            run(
                "cargo test --release",
                workspace.clone(),
                CommandProfile::Clean,
                false,
            ),
            run("cargo test", workspace.clone(), CommandProfile::User, false),
            run("cargo test", workspace.clone(), CommandProfile::Clean, true),
            CommandRequest::Run {
                command: "cargo test".to_owned(),
                cwd: workspace.clone(),
                profile: CommandProfile::Clean,
                shell: Some(PathBuf::from("/bin/zsh")),
                terminal: false,
            },
        ] {
            assert_eq!(
                policy.admit_command(&different),
                Admission::ApprovalRequired,
                "{different:?}"
            );
        }
    }

    #[test]
    fn forgetting_approvals_drops_every_remembered_grant() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(root.join("a.txt"), "text\n").unwrap();
        let policy = PermissionPolicy::new(PermissionMode::Ask, &workspace);
        let command = CommandRequest::Run {
            command: "cargo test".to_owned(),
            cwd: workspace.clone(),
            profile: CommandProfile::User,
            shell: None,
            terminal: false,
        };
        let change = FileMutation {
            target: workspace.join("a.txt"),
            state: FileMutationState::Changes,
        };
        approve_always(&policy, GatedAction::Call(&read("../a.txt")));
        approve_always(&policy, GatedAction::Command(&command));
        approve_always(&policy, GatedAction::FileMutation(&change));
        assert_ne!(policy.admit(&read("../a.txt")), Admission::ApprovalRequired);
        policy.forget_approvals();
        assert_eq!(policy.admit(&read("../a.txt")), Admission::ApprovalRequired);
        assert_eq!(policy.admit_command(&command), Admission::ApprovalRequired);
        assert_eq!(
            policy.admit_file_mutation(&change),
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
