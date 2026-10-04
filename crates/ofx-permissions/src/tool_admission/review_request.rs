use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ofx_contract::{
    CommandProfile, CommandRequest, FileChange, FileChangeStats, FileMutation, GatedAction,
    ReviewRequest,
};
use ofx_exec::{Environment, Profile, configured_login_shell, environment};
use ofx_markdown::FileReview;
use ofx_shell::{tokenize_argv, unsafe_compound_indicator};
use ofx_workspace::current_branch;

use crate::auto_classifier::{Action, ReviewSubject, Target, select_prior_tool_results};

const TARGET_ROLE: &str = "target";
const PARENT_ROLE: &str = "parent";
const ENVIRONMENT_IDENTITY_PREFIX: &str = "@fx-terminal-env:";
const TERMINAL_IDENTITY_PREFIX: &str = "@fx-shell-mode:tty:";

pub(super) fn review_subject<'a>(
    request: &'a ReviewRequest<'a>,
    trusted_root_context: &'a str,
) -> Option<ReviewSubject<'a>> {
    let call = request.call;
    let (action, targets) = match request.action {
        GatedAction::Command(request @ CommandRequest::Run { command, cwd, .. }) => (
            Action::Command { command, cwd },
            vec![command_target(request)?],
        ),
        GatedAction::Command(CommandRequest::SendInput { .. }) => (
            Action::ShellInput {
                arguments_json: &call.arguments,
            },
            vec![named_target(&call.name)],
        ),
        GatedAction::FileMutation(mutation) => {
            let file = request.file?;
            (file_action(&call.name, file), file_targets(mutation, file))
        }
        GatedAction::Command(CommandRequest::Observe | CommandRequest::Stop)
        | GatedAction::Call(_) => (
            Action::Tool {
                tool_name: &call.name,
                arguments_json: &call.arguments,
            },
            vec![named_target(&call.name)],
        ),
    };
    let proven_current_branch = match &action {
        Action::Command { command, cwd } => proven_current_branch(command, cwd),
        _ => None,
    };
    Some(ReviewSubject {
        model: request.model,
        batch: request.batch,
        call,
        trusted_root_context,
        prior_tool_results: select_prior_tool_results(request.turn, &call.id, request.held),
        proven_current_branch,
        targets,
        action,
    })
}

fn command_target(request: &CommandRequest) -> Option<Target> {
    let CommandRequest::Run {
        command,
        cwd,
        profile,
        shell,
        terminal,
        ..
    } = request
    else {
        return None;
    };
    let profile = match profile {
        CommandProfile::Clean => Profile::Clean,
        CommandProfile::User => Profile::User,
    };
    let environment = match shell {
        Some(shell) => match profile {
            Profile::Clean => Environment::Clean(shell.clone()),
            Profile::User => Environment::User(shell.clone()),
        },
        None => environment(configured_login_shell().as_deref(), Some(profile)).ok()?,
    };
    let (label, shell) = match &environment {
        Environment::Clean(shell) => ("clean", shell),
        Environment::User(shell) => ("user", shell),
    };
    let shell = shell.as_os_str().as_bytes();
    let mut path = cwd.as_os_str().as_bytes().to_vec();
    path.extend_from_slice(b"::");
    if *terminal {
        path.extend_from_slice(TERMINAL_IDENTITY_PREFIX.as_bytes());
    }
    path.extend_from_slice(ENVIRONMENT_IDENTITY_PREFIX.as_bytes());
    path.extend_from_slice(format!("{label}:{}:", shell.len()).as_bytes());
    path.extend_from_slice(shell);
    path.extend_from_slice(b"::");
    path.extend_from_slice(command.as_bytes());
    Some(Target {
        role: TARGET_ROLE,
        path,
    })
}

fn named_target(name: &str) -> Target {
    Target {
        role: TARGET_ROLE,
        path: name.as_bytes().to_vec(),
    }
}

fn file_action<'a>(tool_name: &'a str, file: &'a FileChange<'a>) -> Action<'a> {
    let review = FileReview::new(file.before.unwrap_or_default(), file.after);
    if let Some(line_counts) = file.line_counts {
        let _ = line_counts.set(FileChangeStats::from_lines(
            review.additions(),
            review.deletions(),
        ));
    }
    Action::FileMutation {
        tool_name,
        display_path: &file.display_path,
        preimage_present: file.before.is_some(),
        review,
    }
}

fn file_targets(mutation: &FileMutation, file: &FileChange<'_>) -> Vec<Target> {
    let target = Target {
        role: TARGET_ROLE,
        path: mutation.target.as_os_str().as_bytes().to_vec(),
    };
    let parents = file.parents.iter().map(|parent| Target {
        role: PARENT_ROLE,
        path: parent.as_os_str().as_bytes().to_vec(),
    });
    [target].into_iter().chain(parents).collect()
}

fn proven_current_branch(command: &str, cwd: &Path) -> Option<String> {
    let expected = direct_git_push_branch(command)?;
    current_branch(cwd).filter(|branch| *branch == expected)
}

fn direct_git_push_branch(command: &str) -> Option<String> {
    if unsafe_compound_indicator(command) {
        return None;
    }
    let tokens = tokenize_argv(command).ok()?;
    let [git, push, .., branch] = tokens.as_slice() else {
        return None;
    };
    if tokens.len() < 4
        || git.value != "git"
        || push.value != "push"
        || tokens.iter().any(|token| token.operator)
        || branch.value.is_empty()
        || branch.value.starts_with('-')
        || branch.value == "HEAD"
        || !literal_shell_token(branch.raw)
    {
        return None;
    }
    Some(branch.value.clone())
}

fn literal_shell_token(raw: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for byte in raw.bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if !in_single => escaped = true,
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            _ if in_single => {}
            b'$' | b'`' => return false,
            b'*' | b'?' | b'[' | b'~' if !in_double => return false,
            _ => {}
        }
    }
    !escaped && !in_single && !in_double
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_reviews_record_their_line_counts_for_the_change_they_review() {
        let line_counts = std::sync::OnceLock::new();
        let change = FileChange {
            display_path: "note.txt".to_owned(),
            before: Some(b"a\nb\nc\n"),
            after: b"a\nx\nc\nd\n",
            parents: Vec::new(),
            line_counts: Some(&line_counts),
        };
        let Action::FileMutation { review, .. } = file_action("edit_file", &change) else {
            unreachable!()
        };
        assert_eq!(
            line_counts.get(),
            Some(&FileChangeStats {
                additions: 2,
                deletions: 1,
            })
        );
        assert_eq!((review.additions(), review.deletions()), (2, 1));
        let unshared = FileChange {
            line_counts: None,
            ..change
        };
        assert!(matches!(
            file_action("edit_file", &unshared),
            Action::FileMutation { .. }
        ));
    }

    #[test]
    fn direct_git_push_branch_proof_accepts_only_explicit_literal_operands() {
        assert_eq!(
            direct_git_push_branch("git push origin feature/media-ui").as_deref(),
            Some("feature/media-ui")
        );
        assert_eq!(
            direct_git_push_branch("git push origin 'feature/media-ui'").as_deref(),
            Some("feature/media-ui")
        );
        for command in [
            "rtk git push origin feature/media-ui",
            "git push origin HEAD",
            "git push origin $BRANCH",
            "git push origin \"${BRANCH}\"",
            "git push origin `current-branch`",
            "git push origin $(current-branch)",
            "git push origin feature/*",
            "git push origin feature/media-ui && printf done",
            "git push origin feature/media-ui > log",
            "git push origin feature/media-ui | cat",
            "git push origin --force",
            "git push main",
        ] {
            assert_eq!(direct_git_push_branch(command), None, "{command}");
        }
    }

    fn run(profile: CommandProfile, shell: Option<&str>, terminal: bool) -> CommandRequest {
        CommandRequest::Run {
            command: "touch marker".to_owned(),
            cwd: "/workspace".into(),
            profile,
            shell: shell.map(Into::into),
            terminal,
            reload: false,
        }
    }

    fn target(request: &CommandRequest) -> Option<String> {
        command_target(request).map(|target| String::from_utf8(target.path).unwrap())
    }

    #[test]
    fn command_targets_bind_the_cwd_and_the_shell_environment_as_upstream() {
        assert_eq!(
            target(&run(CommandProfile::User, Some("/bin/zsh"), false)).as_deref(),
            Some("/workspace::@fx-terminal-env:user:8:/bin/zsh::touch marker")
        );
        assert_eq!(
            target(&run(CommandProfile::Clean, Some("/usr/bin/bash"), true)).as_deref(),
            Some(
                "/workspace::@fx-shell-mode:tty:@fx-terminal-env:clean:13:/usr/bin/bash::touch marker"
            )
        );
        if let Some(login) = target(&run(CommandProfile::Clean, None, false)) {
            assert!(
                login.starts_with("/workspace::@fx-terminal-env:clean:"),
                "{login}"
            );
            assert!(login.ends_with("::touch marker"), "{login}");
        }
        assert_eq!(target(&CommandRequest::Stop), None);
    }

    #[test]
    fn proven_current_branch_matches_the_checked_out_branch_only() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/feature/media-ui\n").unwrap();
        assert_eq!(
            proven_current_branch("git push origin feature/media-ui", &root).as_deref(),
            Some("feature/media-ui")
        );
        assert_eq!(proven_current_branch("git push origin main", &root), None);
        assert_eq!(
            proven_current_branch("git push origin feature/media-ui", Path::new("relative")),
            None
        );
    }
}
