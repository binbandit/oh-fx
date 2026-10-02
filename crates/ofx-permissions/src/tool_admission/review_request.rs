use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ofx_contract::{CommandRequest, FileChange, FileMutation, GatedAction, ReviewRequest};
use ofx_markdown::FileReview;
use ofx_shell::{tokenize_argv, unsafe_compound_indicator};
use ofx_workspace::current_branch;

use crate::auto_classifier::{Action, ReviewSubject, Target, select_prior_tool_results};

const TARGET_ROLE: &str = "target";
const PARENT_ROLE: &str = "parent";

pub(super) fn review_subject<'a>(
    request: &'a ReviewRequest<'a>,
    trusted_root_context: &'a str,
) -> Option<ReviewSubject<'a>> {
    let call = request.call;
    let (action, targets) = match request.action {
        GatedAction::Command(CommandRequest::Run { command, cwd, .. }) => (
            Action::Command { command, cwd },
            vec![command_target(cwd, command)],
        ),
        GatedAction::Command(CommandRequest::SendInput { .. }) => (
            Action::ShellInput {
                arguments_json: &call.arguments,
            },
            Vec::new(),
        ),
        GatedAction::FileMutation(mutation) => {
            let file = request.file.as_ref()?;
            (file_action(&call.name, file), file_targets(mutation, file))
        }
        GatedAction::Command(CommandRequest::Observe | CommandRequest::Stop)
        | GatedAction::Call(_) => (
            Action::Tool {
                tool_name: &call.name,
                arguments_json: &call.arguments,
            },
            Vec::new(),
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

fn command_target(cwd: &Path, command: &str) -> Target {
    let mut path = cwd.as_os_str().as_bytes().to_vec();
    path.extend_from_slice(b"::");
    path.extend_from_slice(command.as_bytes());
    Target {
        role: TARGET_ROLE,
        path,
    }
}

fn file_action<'a>(tool_name: &'a str, file: &'a FileChange<'a>) -> Action<'a> {
    Action::FileMutation {
        tool_name,
        display_path: &file.display_path,
        preimage_present: file.before.is_some(),
        review: FileReview::new(file.before.unwrap_or_default(), file.after),
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
