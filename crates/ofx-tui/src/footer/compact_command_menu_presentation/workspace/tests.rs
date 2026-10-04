use std::path::PathBuf;

use ofx_contract::DirectoryAccess;

use super::*;
use crate::row_text::Paint;

fn theme() -> Theme {
    Theme::builtin(false, true, true)
}

fn texts(rows: &[Row]) -> Vec<String> {
    rows.iter().map(Row::text).collect()
}

fn entry(path: &str, saved: bool, command_line: bool, available: bool) -> WorkspaceMenuEntry {
    WorkspaceMenuEntry {
        path: PathBuf::from(path),
        saved,
        command_line,
        access: DirectoryAccess::of(available, available),
    }
}

fn menu(saved_suppressed: bool) -> WorkspaceMenu {
    WorkspaceMenu {
        primary: PathBuf::from("/tmp/project"),
        saved_suppressed,
        limit: 16,
        entries: vec![
            entry("/tmp/launch", false, true, true),
            entry("/tmp/shared", true, false, true),
            entry("/tmp/gone", true, true, false),
        ],
    }
}

fn columns(label: &str, info: &str) -> String {
    format!("{label:<29}{info}")
}

#[test]
fn the_menu_pins_the_summary_above_one_row_per_directory_and_action() {
    let menu = menu(false);
    assert_eq!(workspace_desired_row_count(&menu), 10);
    assert_eq!(
        texts(&workspace_menu_rows(&theme(), &menu, 0, 10, 80)),
        [
            "Workspace".to_owned(),
            String::new(),
            format!("{:<28}/tmp/project", "  Primary"),
            format!("{:<28}3 / 16", "  Additional directories"),
            String::new(),
            columns("❯ Add directory…", "Grant access to another directory"),
            columns("  /tmp/launch", "Active · Launch only"),
            columns("  /tmp/shared", "Active · Saved"),
            columns("  /tmp/gone", "Unavailable · Saved + launch"),
            columns(
                "  Clear saved directories",
                "Remove every saved additional directory"
            ),
        ]
    );
}

#[test]
fn only_the_selected_action_is_highlighted_and_suppression_is_summarized() {
    let theme = theme();
    let rows = workspace_menu_rows(&theme, &menu(true), 2, 10, 80);
    assert_eq!(
        rows[3].text(),
        format!(
            "{:<28}3 / 16 · Saved roots suppressed",
            "  Additional directories"
        )
    );
    let painted = |row: &Row| -> Vec<(String, Paint)> {
        row.segments()
            .iter()
            .filter(|segment| !segment.text.trim().is_empty())
            .map(|segment| (segment.text.trim().to_owned(), segment.paint))
            .collect()
    };
    assert_eq!(
        painted(&rows[7]),
        [
            ("❯ /tmp/shared".to_owned(), theme.system_notice_label),
            ("Active · Saved".to_owned(), theme.dim),
        ]
    );
    assert_eq!(rows[5].segments()[0].paint, theme.dim);
    assert_eq!(
        painted(&rows[2])[0],
        ("Primary".to_owned(), theme.system_notice_label)
    );
}

#[test]
fn short_menus_keep_the_selection_in_view_under_the_title() {
    let menu = menu(false);
    assert_eq!(
        texts(&workspace_menu_rows(&theme(), &menu, 4, 3, 80)),
        [
            "Workspace".to_owned(),
            columns("  /tmp/gone", "Unavailable · Saved + launch"),
            columns(
                "❯ Clear saved directories",
                "Remove every saved additional directory"
            ),
        ]
    );
    assert_eq!(
        texts(&workspace_menu_rows(&theme(), &menu, 2, 1, 80)),
        [columns("❯ /tmp/shared", "Active · Saved")]
    );
    assert_eq!(
        texts(&workspace_menu_rows(&theme(), &menu, 0, 7, 80))[5..],
        [
            columns("❯ Add directory…", "Grant access to another directory"),
            columns("  /tmp/launch", "Active · Launch only"),
        ]
    );
}

#[test]
fn narrow_menus_drop_the_info_column_and_stay_within_the_width() {
    let menu = WorkspaceMenu {
        entries: vec![entry("/tmp/\u{1b}[2J", true, false, true)],
        ..menu(false)
    };
    let rows = workspace_menu_rows(&theme(), &menu, 1, 8, 30);
    assert_eq!(rows[6].text(), "❯ /tmp/\\x1b[2J");
    assert_eq!(rows[5].text(), "  Add directory…");
    for width in [1, 2, 3, 12, 30, 52] {
        for row in workspace_menu_rows(&theme(), &menu, 0, 8, width) {
            assert!(row.width() <= width, "{width}: {:?}", row.text());
        }
    }
}

#[test]
fn the_hint_shortens_to_fit() {
    assert_eq!(
        workspace_menu_hint_row(&theme(), 80).text(),
        "↑↓ navigate     enter use     esc close"
    );
    assert_eq!(
        workspace_menu_hint_row(&theme(), 30).text(),
        "↑↓ move  enter use  esc"
    );
    assert_eq!(workspace_menu_hint_row(&theme(), 4).text(), "ente");
}
