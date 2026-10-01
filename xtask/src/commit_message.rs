use std::path::Path;

use crate::{attribution, conventional, workspace_files};

const GIT_SCISSORS_LINE: &str = "# ------------------------ >8 ------------------------";

pub(crate) fn check(path: &Path) -> Result<(), String> {
    let message = workspace_files::read(path)?;
    let message = above_scissors(&message);
    conventional::check_subject(subject(message))?;
    let author = workspace_files::git(&["var", "GIT_AUTHOR_IDENT"])?;
    let committer = workspace_files::git(&["var", "GIT_COMMITTER_IDENT"])?;
    attribution::check_text(&format!(
        "{message}\nAuthor: {author}\nCommitter: {committer}"
    ))
}

fn above_scissors(message: &str) -> &str {
    message
        .find(GIT_SCISSORS_LINE)
        .map_or(message, |position| &message[..position])
}

fn subject(message: &str) -> &str {
    message
        .lines()
        .find(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_the_verbose_diff_below_the_scissors_line() {
        let message = format!("fix: hook\n\n{GIT_SCISSORS_LINE}\n+Co-authored-by: in a diff\n");
        assert_eq!(above_scissors(&message), "fix: hook\n\n");
    }

    #[test]
    fn takes_the_subject_after_template_comments() {
        assert_eq!(
            subject("# template\n\nfeat: add ask\n\nbody\n"),
            "feat: add ask"
        );
        assert_eq!(subject("\n"), "");
    }
}
