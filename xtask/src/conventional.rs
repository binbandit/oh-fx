use std::path::Path;

use regex::Regex;

use crate::workspace_files;

const SUBJECT_PATTERN: &str = r"^(build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test)(\([a-z0-9][a-z0-9-]*\))?!?: \S";

const GIT_GENERATED_PREFIXES: &[&str] = &["Merge ", "fixup! ", "squash! ", "amend! "];

pub(crate) fn check_subjects_file(path: &Path) -> Result<(), String> {
    let subjects = workspace_files::read(path)?;
    let findings: Vec<String> = subjects
        .lines()
        .filter(|subject| !subject.trim().is_empty() && !is_acceptable_commit_subject(subject))
        .map(str::to_owned)
        .collect();
    report(&findings)
}

pub(crate) fn check_title_file(path: &Path) -> Result<(), String> {
    let title = workspace_files::read(path)?;
    let title = title.trim();
    if is_conventional(title) {
        Ok(())
    } else {
        report(&[title.to_owned()])
    }
}

pub(crate) fn check_commit_subject(subject: &str) -> Result<(), String> {
    if is_acceptable_commit_subject(subject) {
        Ok(())
    } else {
        report(&[subject.to_owned()])
    }
}

fn is_acceptable_commit_subject(subject: &str) -> bool {
    GIT_GENERATED_PREFIXES
        .iter()
        .any(|prefix| subject.starts_with(prefix))
        || is_conventional(subject)
}

fn is_conventional(subject: &str) -> bool {
    Regex::new(SUBJECT_PATTERN)
        .expect("subject pattern is valid")
        .is_match(subject)
}

fn report(findings: &[String]) -> Result<(), String> {
    crate::report(
        "commit subjects and PR titles must follow Conventional Commits, such as `feat(scope): add a thing`",
        findings,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_conventional_subjects() {
        for subject in [
            "feat: add ask",
            "fix(upgrade): verify checksums",
            "ci!: require the lint job",
            "docs(architecture): record decisions",
        ] {
            assert!(is_conventional(subject), "{subject}");
        }
    }

    #[test]
    fn accepts_commit_subjects_git_generates() {
        assert!(is_acceptable_commit_subject(
            "Merge branch 'main' into feature"
        ));
        assert!(is_acceptable_commit_subject("fixup! feat: add ask"));
    }

    #[test]
    fn requires_pull_request_titles_to_be_conventional() {
        assert!(!is_conventional("Merge arbitrary title"));
        assert!(!is_conventional("fixup! feat: add ask"));
    }

    #[test]
    fn rejects_other_subjects() {
        for subject in [
            "Add ask",
            "feat:add ask",
            "feature: add ask",
            "feat(Scope): add ask",
            "FEAT: add ask",
        ] {
            assert!(!is_acceptable_commit_subject(subject), "{subject}");
        }
    }
}
