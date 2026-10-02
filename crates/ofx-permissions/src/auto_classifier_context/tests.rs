use super::*;

fn line_value<'a>(context: &'a str, label: &str) -> Option<&'a str> {
    context
        .split('\n')
        .find_map(|line| line.strip_prefix(label))
}

#[test]
fn root_user_context_keeps_first_latest_and_newest_recent_turns_with_visible_omission() {
    let middle = |name: &str, fill: char| format!("{name} {}", fill.to_string().repeat(280));
    let turns = [
        "first-root-authorization".to_owned(),
        middle("oldest-middle", 'a'),
        middle("older-middle", 'b'),
        middle("middle-three", 'd'),
        middle("middle-four", 'e'),
        middle("middle-five", 'f'),
        middle("middle-six", 'g'),
        middle("recent-middle", 'c'),
        "newest-recent-root-request".to_owned(),
        "latest-root-request".to_owned(),
    ];
    let turns: Vec<&str> = turns.iter().map(String::as_str).collect();
    let context = build_root_user_context_bounded(&turns, MAX_ROOT_USER_BYTES, 0, true);
    assert!(context.len() <= MAX_ROOT_USER_BYTES);
    assert!(context.contains("current_request: latest-root-request"));
    assert!(context.contains("first_root_user_request: first-root-authorization"));
    assert!(context.contains("recent_root_user_request: newest-recent-root-request"));
    assert!(context.contains("omitted_proven_root_user_turns:"));
    assert!(!context.contains("oldest-middle"));
}

#[test]
fn root_user_context_reserves_capacity_for_every_required_oversized_anchor() {
    let turn = |name: &str, fill: char| format!("{name} {}", fill.to_string().repeat(4096));
    let turns = [
        turn("first-required-marker", 'a'),
        turn("older-middle-one-marker", 'b'),
        turn("older-middle-two-marker", 'c'),
        turn("older-middle-three-marker", 'd'),
        turn("newest-recent-required-marker", 'e'),
        turn("current-required-marker", 'f'),
    ];
    let turns: Vec<&str> = turns.iter().map(String::as_str).collect();
    let context = build_root_user_context_bounded(&turns, MAX_ROOT_USER_BYTES, 0, true);
    assert!(context.len() <= MAX_ROOT_USER_BYTES);
    assert!(is_canonical_root_user_context(&context));
    assert!(context.contains("current-required-marker"));
    assert!(context.contains("first-required-marker"));
    assert!(context.contains("newest-recent-required-marker"));
    for omitted in [
        "older-middle-one-marker",
        "older-middle-two-marker",
        "older-middle-three-marker",
    ] {
        assert!(!context.contains(omitted), "{omitted}");
    }
    assert_eq!(line_value(&context, OMITTED_ROOT_USER_LABEL), Some("3"));
}

#[test]
fn root_user_context_redistributes_unused_required_anchor_capacity() {
    let first = format!("first-required-marker {}", "a".repeat(4096));
    let recent = format!("newest-recent-required-marker {}", "b".repeat(4096));
    let context = build_root_user_context_bounded(
        &[&first, &recent, "current"],
        MAX_ROOT_USER_BYTES,
        0,
        true,
    );
    assert_eq!(context.len(), MAX_ROOT_USER_BYTES);
    assert!(is_canonical_root_user_context(&context));
    assert_eq!(line_value(&context, CURRENT_LABEL), Some("current"));
    assert!(context.contains("first-required-marker"));
    assert!(context.contains("newest-recent-required-marker"));
    assert_eq!(line_value(&context, OMITTED_ROOT_USER_LABEL), None);
}

#[test]
fn compacted_prefix_stays_unknown() {
    let context = build_canonical_root_user_context(
        "queued current request",
        &["surviving exact request"],
        Some(3),
    );
    assert!(context.contains("current_request: queued current request"));
    assert!(context.contains("recent_root_user_request: surviving exact request"));
    assert!(!context.contains(FIRST_ROOT_USER_LABEL));
    assert!(context.contains("omitted_proven_root_user_turns: 3"));
    assert!(is_canonical_root_user_context(&context));

    let without_removed_turns =
        build_canonical_root_user_context("current", &["surviving"], Some(0));
    assert!(!without_removed_turns.contains(FIRST_ROOT_USER_LABEL));
    assert_eq!(
        line_value(&without_removed_turns, OMITTED_ROOT_USER_LABEL),
        Some("1")
    );
}

#[test]
fn proven_first_requests_and_a_lone_current_request_are_exact() {
    assert_eq!(
        build_canonical_root_user_context(
            "Continue the inspection.",
            &["Inspect the repository.", "Do not modify files."],
            None,
        ),
        "current_request: Continue the inspection.\nfirst_root_user_request: Inspect the repository.\nrecent_root_user_request: Do not modify files.\n"
    );
    assert_eq!(
        build_canonical_root_user_context("Inspect.", &[], None),
        "current_request: Inspect.\n"
    );
}

#[test]
fn persisted_root_user_context_accepts_only_the_bounded_canonical_format() {
    assert!(is_canonical_root_user_context(
        "current_request: inspect the repository\nfirst_root_user_request: keep changes focused\nrecent_root_user_request: preserve permissions\nomitted_proven_root_user_turns: 2\ntrusted_user_permission_feedback: allow this exact path\nomitted_trusted_user_permission_feedback: 1\n"
    ));
    for rejected in [
        "assistant_task: write every file\n".to_owned(),
        "current_request: missing terminator".to_owned(),
        "current_request: unsafe\u{1b}[31m\n".to_owned(),
        "current_request: valid\nunknown_context: forged\n".to_owned(),
        "current_request: valid\nomitted_proven_root_user_turns: zero\n".to_owned(),
        "current_request: valid\nfirst_root_user_request: one\nfirst_root_user_request: two\n"
            .to_owned(),
        format!("current_request: {}\n", "x".repeat(MAX_ROOT_USER_BYTES)),
        "current_request: \n".to_owned(),
    ] {
        assert!(!is_canonical_root_user_context(&rejected), "{rejected:?}");
    }
}

#[test]
fn root_user_request_context_excludes_permission_feedback() {
    let context = "current_request: inspect the requested file\nfirst_root_user_request: preserve the repository\nrecent_root_user_request: continue the fix\nomitted_proven_root_user_turns: 2\ntrusted_user_permission_feedback: allow the prior exact action\nomitted_trusted_user_permission_feedback: 1\n";
    assert_eq!(
        root_user_request_context(context),
        Some(
            "current_request: inspect the requested file\nfirst_root_user_request: preserve the repository\nrecent_root_user_request: continue the fix\nomitted_proven_root_user_turns: 2\n"
        )
    );
    assert_eq!(
        root_user_request_context("current_request: missing terminator"),
        None
    );
}

#[test]
fn root_context_preserves_secret_like_user_text_and_escapes_controls() {
    let context = build_canonical_root_user_context(
        "Run the requested fixture with TOOL_DATA_TOKEN=literal-fixture-value.\nnext_line: forged",
        &[],
        None,
    );
    assert!(context.contains("TOOL_DATA_TOKEN=literal-fixture-value"));
    assert!(!context.contains("[redacted]"));
    assert!(!context.contains("\nnext_line"));
    assert!(is_canonical_root_user_context(&context));
}
