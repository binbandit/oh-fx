use std::path::PathBuf;

use super::*;
use crate::skill_contract::SkillSource;

fn match_explicit_skill_indices(prompt: &str, skills: &[Skill]) -> Vec<usize> {
    collect_explicit_skill_selections(prompt, skills)
        .into_iter()
        .filter_map(|selection| match selection {
            ExplicitSelection::Skill(index) => Some(index),
            ExplicitSelection::Ambiguous(_) => None,
        })
        .collect()
}

fn static_skill(name: &str, path: &str) -> Skill {
    Skill {
        name: name.to_string(),
        description: String::new(),
        path: PathBuf::from(path),
        source: SkillSource::WorkspaceShared,
        read_authority: None,
    }
}

fn review_and_release() -> [Skill; 2] {
    [
        static_skill("review", "/skills/review"),
        static_skill("release", "/skills/release"),
    ]
}

#[test]
fn explicit_skill_matching_excludes_at_path_regions_in_dollar_and_natural_references() {
    let skills = review_and_release();
    for prompt in [
        "@./$review.txt",
        "@\"./$review.txt\"",
        "@\"./a\\\"$review.txt\"",
        "@\"./a\\\\$review.txt\"",
        "Use @\"review skill\"",
        "Use @review skill",
        "@\"broken$review",
        "@\"a.png\"invalid$review",
    ] {
        assert!(
            collect_explicit_skill_selections(prompt, &skills).is_empty(),
            "{prompt}"
        );
    }
    for prompt in [
        "$release @./$review.txt",
        "Use release skill @\"./$review.txt\"",
        "@./$review.txt use $release",
    ] {
        assert_eq!(
            match_explicit_skill_indices(prompt, &skills),
            [1],
            "{prompt}"
        );
    }
}

#[test]
fn explicit_skill_matching_finds_multiple_mentions_in_request_order() {
    let skills = review_and_release();
    assert_eq!(
        match_explicit_skill_indices(
            "Please work with $release, then $review and $release again.",
            &skills
        ),
        [1, 0]
    );
}

#[test]
fn explicit_dollar_references_do_not_select_a_composite_natural_language_name() {
    let skills = [
        static_skill("release", "/a"),
        static_skill("review", "/b"),
        static_skill("release and review", "/c"),
    ];
    assert_eq!(
        match_explicit_skill_indices("Use $release and $review skill", &skills),
        [0, 1]
    );
}

#[test]
fn explicit_skill_matching_accepts_sigils_and_verbs_without_fuzzy_activation() {
    let skills = [
        static_skill("review", "/review"),
        static_skill("release-notes", "/release-notes"),
    ];
    for (prompt, expected) in [
        ("$review inspect this patch", 0),
        ("please use the release-notes skill", 1),
        ("apply review skill", 0),
        ("activate the review skill", 0),
        ("invoke release_notes skill", 1),
        ("run the release notes skill for this patch", 1),
        ("/review inspect this patch", 0),
    ] {
        assert_eq!(
            match_explicit_skill_indices(prompt, &skills),
            [expected],
            "{prompt}"
        );
    }
    assert!(
        match_explicit_skill_indices("review this patch and write release notes", &skills)
            .is_empty()
    );
}

#[test]
fn explicit_skill_matching_parses_natural_language_once_for_a_large_catalog() {
    let skills: Vec<Skill> = (0..128)
        .map(|index| {
            if index == 73 {
                static_skill("release-notes", "/release-notes")
            } else {
                static_skill(&format!("catalog-skill-{index}"), &format!("/{index}"))
            }
        })
        .collect();
    assert_eq!(
        match_explicit_skill_indices(
            "Please invoke the release_notes skill for this patch.",
            &skills
        ),
        [73]
    );
}

#[test]
fn explicit_skill_matching_ignores_negated_quoted_and_pasted_references() {
    let skills = [static_skill("review", "/review")];
    for prompt in [
        "Do not use the review skill.",
        "Please do not apply the review skill.",
        "Explain why \"use the review skill\" is unsafe.",
        "\"use the review skill\" is only an example.",
        "A pasted example says: use the review skill.",
        "Please use the reviewer skill.",
        "Please use the review skills.",
        "Please use the review skillful workflow.",
        "Do not $review this patch.",
        "Do not use $review for this patch.",
        "The escaped \\$review stays text.",
        "The quoted \"$review\" stays text.",
        "The literal `$review` should remain text.",
    ] {
        assert!(
            match_explicit_skill_indices(prompt, &skills).is_empty(),
            "{prompt}"
        );
    }
}

#[test]
fn explicit_skill_matching_preserves_never_negation_across_invocation_forms() {
    let skills = [static_skill("acceptance-workflow", "/acceptance")];
    for prompt in [
        "Never use $acceptance-workflow. Calculate 17 times 19 and do not modify files.",
        "Never $acceptance-workflow.",
        "Never apply $acceptance-workflow.",
        "Never invoke $acceptance-workflow.",
        "Never run $acceptance-workflow.",
        "Never activate $acceptance-workflow.",
        "Never use the $acceptance-workflow.",
        "NEVER USE $acceptance-workflow.",
    ] {
        assert!(
            match_explicit_skill_indices(prompt, &skills).is_empty(),
            "{prompt}"
        );
    }
    assert_eq!(
        match_explicit_skill_indices(
            "Never modify files; use $acceptance-workflow to review them.",
            &skills
        ),
        [0]
    );
}

#[test]
fn explicit_skill_matching_excludes_markdown_code_spans_with_matching_delimiter_runs() {
    let skills = review_and_release();
    for prompt in [
        "The literal ``$review`` stays text; use $release.",
        "Literal ``a ` $review ` value``; use $release.",
        "Literal `a `` $review `` value`; use $release.",
        "Literal ``a \\` $review value``; use $release.",
        "Literal ``first\n$review\nlast``; use $release.",
    ] {
        assert_eq!(
            match_explicit_skill_indices(prompt, &skills),
            [1],
            "{prompt}"
        );
    }
}

#[test]
fn explicit_skill_matching_excludes_markdown_fenced_code_through_its_closing_fence() {
    let skills = review_and_release();
    for prompt in [
        "Example:\n```sh\nprintf '`'\n$review\n```\nUse $release.",
        "Example:\n~~~sh\n$review\n~~~\nUse $release.",
        "  ~~~~sh\n$review\n~~~\n$review\n  ~~~~~\nUse $release.",
        "```sh\n~~~\n$review\n``` trailing text\n$review\n```\nUse $release.",
        "```sh\n$review\n    ```\n$review\n   ```\nUse $release.",
        "~~~sh\r\n$review\r\n~~~ \t\r\nUse $release.",
        "Use $release.\n~~~\n$review\nNo closing fence",
    ] {
        assert_eq!(
            match_explicit_skill_indices(prompt, &skills),
            [1],
            "{prompt}"
        );
    }
}

#[test]
fn explicit_skill_matching_refuses_ambiguous_duplicate_names() {
    let skills = [
        static_skill("review", "/workspace/review"),
        static_skill("review", "/global/review"),
    ];
    assert!(match_explicit_skill_indices("$review", &skills).is_empty());
    assert_eq!(
        collect_explicit_skill_selections("$review", &skills),
        [ExplicitSelection::Ambiguous("review")]
    );
}

#[test]
fn explicit_skill_matching_reports_each_ambiguous_name_once_in_any_case() {
    let skills = [
        static_skill("Review", "/workspace/review"),
        static_skill("review", "/global/review"),
        static_skill("release", "/global/release"),
    ];
    assert_eq!(
        collect_explicit_skill_selections("/review then $REVIEW and $release", &skills),
        [
            ExplicitSelection::Ambiguous("Review"),
            ExplicitSelection::Skill(2)
        ]
    );
}

#[test]
fn natural_language_references_need_the_exact_normalized_name_before_the_skill_marker() {
    let skills = [
        static_skill("release notes", "/release-notes"),
        static_skill("release", "/release"),
    ];
    let repeated_markers = format!("use the release{}", " skill".repeat(64));
    assert_eq!(
        match_explicit_skill_indices(&repeated_markers, &skills),
        [1]
    );
    assert_eq!(
        match_explicit_skill_indices("Use the RELEASE -- Notes skill, then ship", &skills),
        [0]
    );
    assert!(
        match_explicit_skill_indices("use the release skillful notes skill", &skills).is_empty()
    );
}
