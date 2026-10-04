use ofx_contract::{
    FastModeSetting, ModelCatalogSource, PermissionMode, StatuslineItem, StatuslineToggles,
};

use super::*;
use crate::shell::model_menu::tests::option;

fn theme() -> Theme {
    Theme::builtin(false, true, true)
}

fn snapshot() -> SettingsSnapshot {
    let mut statusline = StatuslineToggles::default();
    statusline.set(StatuslineItem::Context, true);
    SettingsSnapshot {
        model: "zai/glm-5.2".to_owned(),
        effort: "default".to_owned(),
        reasoning_efforts: Vec::new(),
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
        models: None,
    }
}

fn listed(ids: &[&str]) -> CatalogLoad {
    CatalogLoad::Listed {
        models: ids.iter().map(|id| option(id)).collect(),
        source: ModelCatalogSource::ProfileSettings,
    }
}

fn with_models<'a>(
    snapshot: &'a SettingsSnapshot,
    selected: usize,
    menu: &'a ModelMenu,
    catalog: &'a CatalogLoad,
) -> SettingsView<'a> {
    SettingsView {
        selected,
        models: Some(InlineModels { menu, catalog }),
        ..view(snapshot, "")
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
        "Settings 10  [All]  Interface  Agent  Notifications  Advanced"
    );
    assert_eq!(text[1], "");
    assert_eq!(text[2], "  Status line context      off  on");
    assert!(!text[2].contains("Show context usage"));
    assert_eq!(text[6], "  Reasoning effort         default");
    assert_eq!(text[11], "  Prompt history           off  on");
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
    assert_eq!(narrow[0].text(), "Settings 10  [All]");
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
        selected: 9,
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

#[test]
fn the_effort_row_offers_default_and_the_model_s_efforts() {
    let snapshot = SettingsSnapshot {
        effort: "low".to_owned(),
        reasoning_efforts: vec!["low".to_owned(), "high".to_owned()],
        ..snapshot()
    };
    let theme = theme();
    let effort = SettingsView {
        selected: 4,
        ..view(&snapshot, "")
    };
    let rows = settings_menu_rows(&theme, effort, 40, 100);
    assert_eq!(
        rows[6].text(),
        "  Reasoning effort         default  low  high"
    );
    let options: Vec<_> = rows[6]
        .segments()
        .iter()
        .filter(|segment| !segment.text.trim().is_empty())
        .map(|segment| (segment.text.clone(), segment.paint))
        .collect();
    assert_eq!(
        options,
        [
            ("Reasoning effort".to_owned(), theme.selected_completion),
            ("default".to_owned(), theme.dim),
            ("low".to_owned(), theme.selected_completion),
            ("high".to_owned(), theme.dim),
        ]
    );
}

#[test]
fn model_choices_stay_visible_under_the_model_row_within_the_budget() {
    let snapshot = snapshot();
    let menu = ModelMenu::default();
    let catalog = listed(&["provider/one", "provider/two", "provider/three"]);
    let models = with_models(&snapshot, 2, &menu, &catalog);
    assert_eq!(
        texts(&settings_menu_rows(&theme(), models, 8, 100)),
        [
            "Settings 10  [All]  Interface  Agent  Notifications  Advanced",
            "",
            "  Status line workspace    off  on",
            "  Model                    zai/glm-5.2",
            "                           provider/one",
            "                           provider/two",
            "                           provider/three",
            "  Reasoning effort         default",
        ]
    );
    assert_eq!(visible_model_items_for_budget(models, 8), 3);
    assert_eq!(visible_model_items_for_budget(models, 6), 2);
    assert_eq!(
        texts(&settings_menu_rows(&theme(), models, 3, 100))[2],
        "                           provider/one"
    );
    assert_eq!(visible_model_items_for_budget(models, 3), 1);
}

#[test]
fn the_inline_models_open_at_the_selected_row_and_follow_the_model_selection() {
    let snapshot = snapshot();
    let ids = ["v/0", "v/1", "v/2", "v/3", "v/4", "v/5", "v/6", "v/7"];
    let catalog = listed(&ids);
    let mut menu = ModelMenu::default();
    menu.selected = 7;
    let theme = theme();
    let rows = settings_menu_rows(&theme, with_models(&snapshot, 3, &menu, &catalog), 40, 100);
    let text = texts(&rows);
    assert_eq!(text[2], "  Model                    zai/glm-5.2");
    let shown: Vec<_> = text[3..9].iter().map(|row| row.trim()).collect();
    assert_eq!(shown, ["v/2", "v/3", "v/4", "v/5", "v/6", "v/7"]);
    assert_eq!(text[9], "  Reasoning effort         default");
    assert_eq!(rows[8].segments()[1].paint, theme.selected_completion);
    assert_eq!(rows[7].segments()[1].paint, theme.dim);
}

#[test]
fn a_short_inline_list_keeps_the_selected_model_in_view() {
    let snapshot = snapshot();
    let ids = ["v/0", "v/1", "v/2", "v/3", "v/4", "v/5", "v/6", "v/7"];
    let catalog = listed(&ids);
    let mut menu = ModelMenu::default();
    menu.selected = 7;
    let theme = theme();
    let models = with_models(&snapshot, 3, &menu, &catalog);
    let rows = settings_menu_rows(&theme, models, 5, 100);
    let text = texts(&rows);
    assert_eq!(text[2], "  Model                    zai/glm-5.2");
    let shown: Vec<_> = text[3..].iter().map(|row| row.trim()).collect();
    assert_eq!(shown, ["v/6", "v/7"]);
    assert_eq!(rows[4].segments()[1].paint, theme.selected_completion);
    menu.window_start = 6;
    menu.selected = 6;
    let rows = texts(&settings_menu_rows(
        &theme,
        with_models(&snapshot, 3, &menu, &catalog),
        5,
        100,
    ));
    let shown: Vec<_> = rows[3..].iter().map(|row| row.trim()).collect();
    assert_eq!(shown, ["v/6", "v/7"]);
}

#[test]
fn the_inline_models_say_when_they_are_loading_missing_or_unavailable() {
    let snapshot = snapshot();
    let menu = ModelMenu::default();
    let row = |catalog: &CatalogLoad, menu: &ModelMenu| {
        texts(&settings_menu_rows(
            &theme(),
            with_models(&snapshot, 3, menu, catalog),
            40,
            100,
        ))[3]
            .clone()
    };
    assert_eq!(row(&CatalogLoad::Loading, &menu), "    Loading models…");
    assert_eq!(
        row(&CatalogLoad::Failed(None), &menu),
        "    Models unavailable"
    );
    let mut searched = ModelMenu::default();
    searched.set_query("nothing");
    assert_eq!(row(&listed(&["v/one"]), &searched), "    No models found");
}
