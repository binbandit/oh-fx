use super::*;

const NOW: i64 = 1_000 + 8 * MS_PER_MINUTE;

fn session(id: &str, title: &str, workspace: &str, turns: usize) -> SessionRow {
    SessionRow {
        id: id.to_owned(),
        title: Some(title.to_owned()),
        workspace_root: workspace.to_owned(),
        updated_at_ms: 1_000,
        turns,
    }
}

fn view(rows: &[SessionRow]) -> SessionMenuView<'_> {
    SessionMenuView {
        scope: SessionScope::CurrentWorkspace,
        load: LoadState::Ready,
        rows: rows.iter().collect(),
        has_more: false,
        loading_more: false,
        selected: 0,
        window_start: 0,
        refusal: None,
        now_ms: NOW,
    }
}

fn texts(view: &SessionMenuView<'_>, width: usize, budget: usize) -> Vec<String> {
    menu_frame(view, &Theme::builtin(false, false, false), width, budget)
        .rows
        .iter()
        .map(Row::text)
        .collect()
}

#[test]
fn resume_menu_starts_every_turn_count_in_the_same_column() {
    let rows = [
        session("one", "single turn", "/tmp/resume-catalog", 1),
        session("two", "many turns", "/tmp/resume-catalog", 12),
    ];
    let rendered = texts(&view(&rows), 120, 5);
    let first = rendered[2].find("1 turn").unwrap();
    let second = rendered[3].find("12 turns").unwrap();
    assert_eq!(
        visible_width(&rendered[2][..first]),
        visible_width(&rendered[3][..second])
    );
}

#[test]
fn resume_menu_renders_each_session_on_one_line_with_a_right_metadata_cluster() {
    let rows = [session(
        "one",
        "Redesign resume menu",
        "/Users/example/Developer/Fx/worktrees/resume-catalog",
        24,
    )];
    let rendered = texts(&view(&rows), 120, 4);
    assert_eq!(rendered.len(), 3);
    assert_eq!(
        rendered[0],
        "Sessions 1  [Current workspace]  All workspaces"
    );
    assert_eq!(rendered[1], "");
    assert_eq!(
        rendered[2],
        "  Redesign resume menu    resume-catalog · 8m · 24 turns"
    );
    assert!(!rendered[2].contains('●'));
}

#[test]
fn resume_menu_renders_loading_empty_and_failure_states() {
    for (load, text) in [
        (LoadState::Loading, "  Loading sessions…"),
        (LoadState::Ready, "  No sessions found."),
        (LoadState::Failed, "  Unable to load sessions."),
    ] {
        let mut state = view(&[]);
        state.load = load;
        assert_eq!(
            texts(&state, 80, 3),
            ["Sessions 0  [Current workspace]  All workspaces", "", text]
        );
    }
}

#[test]
fn resume_menu_explains_retryable_selected_session_contention() {
    let rows = [session("one", "Selected session", "/w", 1)];
    for (refusal, message) in [
        (
            ResumeRefusal::OpenElsewhere,
            "  This session is open in another oh-fx. Close it there, then press enter to retry.",
        ),
        (
            ResumeRefusal::Unavailable,
            "  Unable to resume this session.",
        ),
    ] {
        let mut state = view(&rows);
        state.refusal = Some(refusal);
        let rendered = texts(&state, 120, 4);
        assert_eq!(rendered[1], message);
        assert!(rendered[2].contains("Selected session"), "{rendered:?}");
    }
}

#[test]
fn resume_menu_keeps_selected_title_and_retry_feedback_in_compact_layouts() {
    let rows = [session("one", "Selected session", "/w", 1)];
    let mut state = view(&rows);
    state.refusal = Some(ResumeRefusal::OpenElsewhere);
    let two = texts(&state, 40, 2);
    assert_eq!(two.len(), 2);
    assert!(two[1].contains("Selected session"), "{two:?}");
    assert!(two[1].ends_with(" · retry"), "{two:?}");
    let three = texts(&state, 40, 3);
    assert!(three[1].contains("Selected session"), "{three:?}");
    assert!(three[2].starts_with("  This session is open"), "{three:?}");
    assert!(three[2].ends_with('…'), "{three:?}");
}

#[test]
fn resume_menu_prioritizes_content_within_a_one_row_budget() {
    let rows = [session("one", "Only row", "/w", 3)];
    assert_eq!(
        texts(&view(&rows), 60, 1),
        ["  Only row    w · 8m · 3 turns"]
    );
}

#[test]
fn resume_menu_clips_long_titles_in_the_middle_before_metadata() {
    let rows = [session(
        "one",
        "a very long title that keeps going until it cannot fit anywhere",
        "/w/project",
        2,
    )];
    let rendered = texts(&view(&rows), 50, 4);
    assert_eq!(
        rendered[2],
        "  a very lon…t anywhere    project · 8m · 2 turns"
    );
    assert!(visible_width(&rendered[2]) < 50);
    let narrow = texts(&view(&rows), 30, 4);
    assert_eq!(narrow[2], "  a very long ti… fit anywhere");
}

#[test]
fn resume_menu_renders_a_navigable_load_more_action() {
    let rows: Vec<SessionRow> = (0..3)
        .map(|index| session(&format!("s{index}"), &format!("Session {index}"), "/w", 1))
        .collect();
    let mut state = view(&rows);
    state.has_more = true;
    let rendered = texts(&state, 60, 22);
    assert_eq!(rendered.len(), 6);
    assert_eq!(rendered[5], "  ↓ Load more");
    state.selected = 3;
    state.loading_more = true;
    let frame = menu_frame(&state, &Theme::builtin(false, false, false), 60, 22);
    assert_eq!(frame.rows[5].text(), "  ↓ Loading more…");
    assert_eq!(
        frame.rows[5].segments()[1].paint,
        Theme::builtin(false, false, false).selected_completion
    );
}

#[test]
fn resume_menu_keeps_the_selected_paginated_action_visible_in_two_rows() {
    let rows = [session("one", "First", "/w", 1)];
    let mut state = view(&rows);
    state.has_more = true;
    assert_eq!(texts(&state, 40, 2)[1], "  First    w · 8m · 1 turn");
    state.selected = 1;
    assert_eq!(
        texts(&state, 40, 2),
        ["Sessions 1  [Current workspace]", "  ↓ Load more"]
    );
}

#[test]
fn the_window_follows_the_selection_without_wrapping() {
    let rows: Vec<SessionRow> = (0..30)
        .map(|index| session(&format!("s{index}"), &format!("Session {index}"), "/w", 1))
        .collect();
    let mut state = view(&rows);
    state.selected = 25;
    let frame = menu_frame(&state, &Theme::builtin(false, false, false), 60, 8);
    assert_eq!(frame.window_start, 20);
    assert!(frame.rows[7].text().starts_with("  Session 25"));
    state.window_start = frame.window_start;
    state.selected = 22;
    let frame = menu_frame(&state, &Theme::builtin(false, false, false), 60, 8);
    assert_eq!(frame.window_start, 20);
    assert!(frame.rows[4].text().starts_with("  Session 22"));
}

#[test]
fn saved_titles_and_workspaces_cannot_drive_the_terminal() {
    let rows = [session(
        "one",
        "evil \u{1b}]2;owned\u{7}\nnext",
        "/w/\u{1b}[2J",
        1,
    )];
    let rendered = texts(&view(&rows), 120, 4);
    assert!(!rendered[2].contains('\u{1b}'), "{rendered:?}");
    assert!(
        rendered[2].contains("evil \\x1b]2;owned\\x07 next"),
        "{rendered:?}"
    );
}

#[test]
fn ages_use_compact_buckets() {
    assert_eq!(compact_age(1_000, 1_000), "now");
    assert_eq!(compact_age(1_000, 1_000 + 3 * MS_PER_MINUTE), "3m");
    assert_eq!(compact_age(1_000, 1_000 + 2 * MS_PER_HOUR), "2h");
    assert_eq!(compact_age(1_000, 1_000 + 3 * MS_PER_DAY), "3d");
    assert_eq!(compact_age(5_000, 1_000), "now");
}
