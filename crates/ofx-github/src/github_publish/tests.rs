use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

use super::*;

fn draft(title: &str, body: &str) -> Draft {
    Draft {
        title: title.to_owned(),
        body: body.to_owned(),
    }
}

fn output(code: i32, stdout: &str, stderr: &str) -> Output {
    Output {
        status: ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

fn published(ok: bool, text: &str) -> Published {
    Published {
        ok,
        text: text.to_owned(),
    }
}

#[test]
fn parse_draft_extracts_title_and_body() {
    assert_eq!(
        parse_draft("Title\n\n## Summary\nhello"),
        Ok(draft("Title", "## Summary\nhello"))
    );
}

#[test]
fn parse_draft_rejects_whitespace_only_input_and_text_without_a_body() {
    for text in [" \n\t ", "Title only", "Done.", "Title only \r\n\t "] {
        assert_eq!(parse_draft(text), Err(InvalidGithubDraft), "{text:?}");
    }
}

#[test]
fn parse_draft_trims_outer_whitespace_and_preserves_body_markdown() {
    assert_eq!(
        parse_draft(" \r\n  Title with space  \r\n\r\n  - one\r\n  - two\r\n\n"),
        Ok(draft("Title with space", "- one\r\n  - two"))
    );
}

#[test]
fn parse_draft_removes_markdown_that_github_shows_literally_in_a_title() {
    for (input, title, body) in [
        (
            "## Add note probe\n\n## Summary\nbody",
            "Add note probe",
            "## Summary\nbody",
        ),
        ("**Add note probe**\n\nbody", "Add note probe", "body"),
        ("# **Add note probe**\n\nbody", "Add note probe", "body"),
        (
            "#123 Keep `code` and **part** bold\n\nbody",
            "#123 Keep `code` and **part** bold",
            "body",
        ),
        (
            "**Bold** and **more**\n\nbody",
            "**Bold** and **more**",
            "body",
        ),
        ("__init__ runs twice\n\nbody", "__init__ runs twice", "body"),
        ("####### Seven\n\nbody", "####### Seven", "body"),
        ("#\tTabbed\n\nbody", "Tabbed", "body"),
    ] {
        assert_eq!(parse_draft(input), Ok(draft(title, body)), "{input:?}");
    }
}

#[test]
fn parse_draft_rejects_a_title_that_is_only_markdown() {
    assert_eq!(
        parse_draft("##\n\n## Summary\nbody"),
        Err(InvalidGithubDraft)
    );
    assert_eq!(parse_draft("**  **\n\nbody"), Err(InvalidGithubDraft));
}

#[test]
fn publish_argv_maps_each_workflow() {
    assert_eq!(
        publish_argv(Workflow::PullRequest, "A title", "Body text"),
        [
            "gh",
            "pr",
            "create",
            "--title",
            "A title",
            "--body",
            "Body text"
        ]
    );
    assert_eq!(
        publish_argv(Workflow::Issue, "A title", "Body text"),
        [
            "gh",
            "issue",
            "create",
            "--title",
            "A title",
            "--body",
            "Body text"
        ]
    );
}

#[test]
fn publish_maps_a_missing_gh_executable_to_a_handled_failure() {
    assert_eq!(
        published_from_spawn_error(io::Error::from(io::ErrorKind::NotFound)).unwrap(),
        published(false, "gh CLI not found in PATH")
    );
    assert!(published_from_spawn_error(io::Error::from(io::ErrorKind::PermissionDenied)).is_err());
}

#[test]
fn publish_reports_gh_failures_from_their_trimmed_stderr_or_a_fallback() {
    assert_eq!(
        published_from_output(&output(1, "ignored", " \nfailed to create\n ")),
        published(false, "failed to create")
    );
    assert_eq!(
        published_from_output(&output(1, "ignored", " \n\t ")),
        published(false, "gh command failed")
    );
    assert_eq!(
        published_from_output(&Output {
            status: ExitStatus::from_raw(9),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }),
        published(false, "gh command failed")
    );
}

#[test]
fn publish_reports_success_from_trimmed_stdout_or_a_fallback() {
    assert_eq!(
        published_from_output(&output(0, " \n\t ", "")),
        published(true, "created successfully")
    );
    assert_eq!(
        published_from_output(&output(
            0,
            " \nhttps://github.com/vercel-labs/fx/pull/1\n ",
            ""
        )),
        published(true, "https://github.com/vercel-labs/fx/pull/1")
    );
}
