use super::*;

const NOW: i64 = 1_000 + 8 * MS_PER_MINUTE;

fn session(id: &str, title: &str, workspace: &str, turns: usize) -> SessionRow {
    SessionRow {
        id: id.to_owned(),
        title: Some(title.to_owned()),
        workspace_root: workspace.to_owned(),
        updated_at_ms: 1_000,
        turns,
        from_fx: false,
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
fn resume_menu_marks_fx_sessions_with_a_trailing_cluster_item_only_when_one_is_listed() {
    let own = session(
        "one",
        "Redesign resume menu",
        "/Users/example/Developer/Fx/worktrees/resume-catalog",
        24,
    );
    let from_fx = SessionRow {
        from_fx: true,
        ..session("two", "Started in fx", "/Users/example/fx-work", 1)
    };
    let rendered = texts(&view(&[own.clone(), from_fx]), 120, 5);
    assert_eq!(
        rendered[2..],
        [
            "  Redesign resume menu    resume-catalog · 8m · 24 turns",
            "  Started in fx           fx-work        · 8m · 1 turn   · fx",
        ]
    );
    assert_eq!(
        texts(&view(&[own]), 120, 4)[2],
        "  Redesign resume menu    resume-catalog · 8m · 24 turns"
    );
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
fn resume_menu_keeps_long_titles_on_one_narrow_row_without_a_workspace_spill_row() {
    let mut row = session(
        "one",
        "Investigate a very long session catalog rendering issue",
        "/Users/example/Developer/Fx/worktrees/a-very-long-session-catalog-worktree",
        1,
    );
    row.updated_at_ms = 1;
    let rows = [row];
    let mut state = view(&rows);
    state.now_ms = 0;
    let frame = menu_frame(&state, &Theme::builtin(false, false, false), 32, 4);
    let title = &frame.rows[2];
    assert!(title.width() <= 32, "{:?}", title.text());
    assert!(title.text().contains('…'), "{:?}", title.text());
    assert!(!title.text().contains('\n'), "{:?}", title.text());
    assert!(
        frame.rows.get(3).is_none_or(|next| next.text().is_empty()),
        "{:?}",
        frame.rows.get(3).map(Row::text)
    );
}

#[test]
fn resume_menu_keeps_shared_prefix_session_titles_distinguishable_when_narrow() {
    let mut alpha = session(
        "alpha",
        "Shared production composer regression investigation session alpha",
        "/workspace",
        1,
    );
    alpha.updated_at_ms = 1;
    let mut beta = session(
        "beta",
        "Shared production composer regression investigation session beta",
        "/workspace",
        1,
    );
    beta.updated_at_ms = 1;
    let theme = Theme::builtin(false, false, false);
    let columns = Columns::default();

    let alpha_row = title_row(&alpha, true, 1, &columns, &theme, 64);
    assert!(
        alpha_row.text().contains("Shared p"),
        "{:?}",
        alpha_row.text()
    );
    assert!(
        alpha_row.text().contains("n alpha"),
        "{:?}",
        alpha_row.text()
    );
    assert!(alpha_row.width() <= 64);

    let beta_row = title_row(&beta, false, 1, &columns, &theme, 64);
    assert!(
        beta_row.text().contains("Shared p"),
        "{:?}",
        beta_row.text()
    );
    assert!(beta_row.text().contains("on beta"), "{:?}", beta_row.text());
    assert!(beta_row.width() <= 64);
}

#[test]
fn resume_menu_metadata_follows_the_widest_matching_title_across_windows() {
    let mut alpha = session("alpha", "A", "/workspace/alpha-worktree", 1);
    alpha.updated_at_ms = MS_PER_MINUTE;
    let mut long = session("long", "Longest title", "/workspace/long-worktree", 100);
    long.updated_at_ms = MS_PER_MINUTE;
    let rows = [alpha, long];
    let mut state = view(&rows);
    state.now_ms = 2 * MS_PER_MINUTE;
    let first = &texts(&state, 100, 3)[2];
    let first_workspace = first.find("alpha-worktree").unwrap();
    let first_turns = first.find("1 turn").unwrap();

    state.selected = 1;
    let second = &texts(&state, 100, 3)[2];
    let second_workspace = second.find("long-worktree").unwrap();
    let second_turns = second.find("100 turns").unwrap();

    assert_eq!(visible_width(&first[..first_workspace]), 19);
    assert_eq!(
        visible_width(&first[..first_workspace]),
        visible_width(&second[..second_workspace])
    );
    assert_eq!(
        visible_width(&first[..first_turns]),
        visible_width(&second[..second_turns])
    );
}

#[test]
fn resume_menu_keeps_a_complete_compact_item_within_a_three_row_budget() {
    let mut row = session("one", "Compact session", "/workspace", 1);
    row.updated_at_ms = 1;
    let rows = [row];
    let state = view(&rows);

    assert_eq!(texts(&state, 80, 3).len(), 3);
    assert_eq!(MenuLayout::build(&state, 3).visible_session_items, 1);
}

#[test]
fn resume_menu_clips_long_titles_before_metadata_across_vt_widths() {
    const LONG_TITLE: &str = "Investigate an extremely long terminal rendering regression while preserving the session metadata columns at every supported width";
    let mut row = session("one", LONG_TITLE, "/workspace/inline-core-menus", 100);
    row.updated_at_ms = MS_PER_MINUTE;
    let rows = [row];
    let mut state = view(&rows);
    state.now_ms = 2 * MS_PER_MINUTE;

    for (width, expect_metadata, expect_full_title) in [
        (40, false, false),
        (80, true, false),
        (120, true, false),
        (220, true, true),
    ] {
        let rendered = texts(&state, width, 3);
        let text = rendered[2].trim_end();
        assert!(visible_width(text) <= width, "{width}: {text:?}");
        assert_eq!(
            text.contains("inline-core-menus"),
            expect_metadata,
            "{width}: {text:?}"
        );
        assert_eq!(
            text.contains(LONG_TITLE),
            expect_full_title,
            "{width}: {text:?}"
        );
        if !expect_full_title {
            assert!(text.contains('…'), "{width}: {text:?}");
        }
    }
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
