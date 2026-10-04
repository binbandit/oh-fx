use ofx_contract::{FastModeSetting, PermissionMode, StatuslineItem, StatuslineToggles};

use super::*;

fn theme() -> Theme {
    Theme::builtin(false, true, true)
}

fn snapshot() -> SettingsSnapshot {
    let mut statusline = StatuslineToggles::default();
    statusline.set(StatuslineItem::Context, true);
    SettingsSnapshot {
        model: "zai/glm-5.2".to_owned(),
        fast_mode: FastModeSetting::On,
        permission_mode: PermissionMode::Ask,
        statusline,
        session_titles: true,
        startup_scrollback: true,
        prompt_history: true,
    }
}

fn view<'a>(snapshot: &'a SettingsSnapshot, query: &'a str) -> SettingsView<'a> {
    SettingsView {
        snapshot,
        category: SettingCategory::All,
        query,
        selected: 0,
        window_start: 0,
    }
}

fn texts(rows: &[Row]) -> Vec<String> {
    rows.iter().map(Row::text).collect()
}

#[test]
fn every_setting_takes_one_row_at_wide_and_narrow_widths() {
    let snapshot = snapshot();
    let wide = settings_menu_rows(&theme(), view(&snapshot, ""), 40, 100);
    assert_eq!(wide.len(), MAX_INLINE_ROWS);
    let text = texts(&wide);
    assert_eq!(
        text[0],
        "Settings 9  [All]  Interface  Agent  Notifications  Advanced"
    );
    assert_eq!(text[1], "");
    assert_eq!(text[2], "  Status line context      off  on");
    assert!(!text[2].contains("Show context usage"));
    assert_eq!(text[10], "  Prompt history           off  on");
    let compact = texts(&settings_menu_rows(&theme(), view(&snapshot, ""), 4, 100));
    assert_eq!(compact.len(), 4);
    assert_eq!(compact[2], "  Status line context      off  on");
    let narrow = settings_menu_rows(&theme(), view(&snapshot, ""), 40, 24);
    assert_eq!(narrow.len(), MAX_INLINE_ROWS);
    assert!(
        narrow[2].text().contains("Status line"),
        "{:?}",
        narrow[2].text()
    );
    for row in &narrow {
        assert!(row.width() <= 24, "{:?}", row.text());
    }
    assert_eq!(narrow[0].text(), "Settings 9  [All]");
}

#[test]
fn values_follow_the_widest_matching_setting_label() {
    let snapshot = snapshot();
    let rows = texts(&settings_menu_rows(&theme(), view(&snapshot, ""), 40, 100));
    let model = rows.iter().find(|row| row.contains("Model")).unwrap();
    assert_eq!(model.find("zai/glm-5.2"), Some(27));
    let permission = rows.iter().find(|row| row.contains("Permission")).unwrap();
    assert_eq!(&permission[27..], "ask  auto  full access");
    let filtered = texts(&settings_menu_rows(
        &theme(),
        view(&snapshot, "history"),
        40,
        100,
    ));
    assert_eq!(filtered[2], "  Prompt history    off  on");
    assert_eq!(filtered[2].find("off"), Some(20));
}

#[test]
fn the_current_value_and_the_selected_row_are_highlighted() {
    let snapshot = snapshot();
    let theme = theme();
    let rows = settings_menu_rows(&theme, view(&snapshot, ""), 40, 100);
    let context: Vec<_> = rows[2]
        .segments()
        .iter()
        .filter(|segment| !segment.text.trim().is_empty())
        .map(|segment| (segment.text.clone(), segment.paint))
        .collect();
    assert_eq!(
        context,
        [
            ("Status line context".to_owned(), theme.selected_completion),
            ("off".to_owned(), theme.dim),
            ("on".to_owned(), theme.selected_completion),
        ]
    );
    assert_eq!(rows[3].segments()[1].paint, theme.dim);
}

#[test]
fn an_empty_search_says_so_and_a_short_menu_follows_the_selection() {
    let snapshot = snapshot();
    let empty = texts(&settings_menu_rows(
        &theme(),
        view(&snapshot, "sound"),
        40,
        100,
    ));
    assert_eq!(
        empty,
        [
            "Settings 0  [All]  Interface  Agent  Notifications  Advanced",
            "",
            "No settings found."
        ]
    );
    let notifications = SettingsView {
        category: SettingCategory::Notifications,
        ..view(&snapshot, "")
    };
    assert_eq!(
        texts(&settings_menu_rows(&theme(), notifications, 40, 100))[2],
        "No settings found."
    );
    let last = SettingsView {
        selected: 8,
        ..view(&snapshot, "")
    };
    let rows = texts(&settings_menu_rows(&theme(), last, 5, 100));
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[4], "  Prompt history           off  on");
    assert_eq!(visible_items_for_budget(last, 5), 3);
}

#[test]
fn the_hint_shrinks_with_the_width_and_warns_of_a_pending_ctrl_c() {
    assert_eq!(
        settings_menu_hint_row(&theme(), 80, false).text(),
        "↑↓ navigate     tab category     ←→ change     esc close"
    );
    assert_eq!(
        settings_menu_hint_row(&theme(), 30, false).text(),
        "tab category  ←→ change  esc"
    );
    assert_eq!(
        settings_menu_hint_row(&theme(), 80, true).text(),
        "press ctrl+c again to exit"
    );
}
