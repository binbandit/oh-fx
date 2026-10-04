use super::*;

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

#[test]
fn the_catalog_groups_searchable_settings_by_category() {
    let snapshot = snapshot();
    assert_eq!(snapshot.filtered_count(SettingCategory::All, ""), 10);
    assert_eq!(snapshot.filtered_count(SettingCategory::Interface, ""), 3);
    assert_eq!(snapshot.filtered_count(SettingCategory::Agent, ""), 5);
    assert_eq!(
        snapshot.filtered_count(SettingCategory::Notifications, ""),
        0
    );
    assert_eq!(snapshot.filtered_count(SettingCategory::Advanced, ""), 2);
    let model = snapshot
        .item_at(SettingCategory::All, "glm 5.2", 0)
        .unwrap();
    assert_eq!(model.id, SettingId::Model);
    assert_eq!(model.value, "zai/glm-5.2");
    let startup = snapshot
        .item_at(SettingCategory::All, "restore startup", 0)
        .unwrap();
    assert_eq!(startup.id, SettingId::StartupScrollback);
    assert_eq!(startup.value, "on");
    assert_eq!(
        snapshot.item_at(SettingCategory::All, "missing preference", 0),
        None
    );
    let by_tag = snapshot
        .item_at(SettingCategory::All, "PROMPT_HISTORY", 0)
        .unwrap();
    assert_eq!(by_tag.label, "Prompt history");
    assert_eq!(
        snapshot.filtered_count(SettingCategory::All, "status line"),
        3
    );
}

#[test]
fn full_access_shows_as_upstream_names_it_and_cycles_from_it() {
    let snapshot = SettingsSnapshot {
        permission_mode: PermissionMode::Yolo,
        ..snapshot()
    };
    assert_eq!(snapshot.value(SettingId::PermissionMode), "full access");
    assert_eq!(
        snapshot.selected_option_index(SettingId::PermissionMode),
        Some(2)
    );
    assert_eq!(
        snapshot
            .cycle_change(SettingId::PermissionMode, 1)
            .unwrap()
            .value,
        "ask"
    );
    assert_eq!(
        snapshot
            .cycle_change(SettingId::PermissionMode, -1)
            .unwrap()
            .value,
        "auto"
    );
}

#[test]
fn choices_are_typed_and_the_model_and_an_unsupported_fast_mode_offer_none() {
    let snapshot = snapshot();
    assert_eq!(snapshot.change_at(SettingId::Model, 0), None);
    assert_eq!(snapshot.option_count(SettingId::PermissionMode), 3);
    assert_eq!(
        snapshot.option_at(SettingId::PermissionMode, 2),
        Some("full access")
    );
    assert_eq!(snapshot.option_count(SettingId::FastMode), 2);
    let unsupported = SettingsSnapshot {
        fast_mode: FastModeSetting::new(false, false),
        ..snapshot.clone()
    };
    assert_eq!(unsupported.option_count(SettingId::FastMode), 0);
    assert_eq!(unsupported.cycle_change(SettingId::FastMode, 1), None);
    let off = snapshot
        .cycle_change(SettingId::StatuslineContext, 1)
        .unwrap();
    assert_eq!(off.setting, SettingId::StatuslineContext);
    assert_eq!(off.enabled(), Some(false));
    assert_eq!(
        snapshot
            .cycle_change(SettingId::SessionTitles, -1)
            .unwrap()
            .enabled(),
        Some(false)
    );
    assert_eq!(snapshot.cycle_change(SettingId::SessionTitles, 0), None);
}

#[test]
fn the_effort_row_offers_default_and_the_model_s_efforts() {
    let offered = SettingsSnapshot {
        effort: "future-tier".to_owned(),
        reasoning_efforts: vec!["future-tier".to_owned(), "high".to_owned()],
        ..snapshot()
    };
    assert_eq!(offered.option_count(SettingId::Effort), 3);
    assert_eq!(offered.option_at(SettingId::Effort, 0), Some("default"));
    assert_eq!(offered.option_at(SettingId::Effort, 1), Some("future-tier"));
    assert_eq!(offered.option_at(SettingId::Effort, 2), Some("high"));
    assert_eq!(offered.option_at(SettingId::Effort, 3), None);
    assert_eq!(offered.selected_option_index(SettingId::Effort), Some(1));
    assert_eq!(
        offered.cycle_change(SettingId::Effort, 1).unwrap().value,
        "high"
    );
    let item = offered
        .item_at(SettingCategory::Agent, "reasoning", 0)
        .unwrap();
    assert_eq!(item.id, SettingId::Effort);
    assert_eq!(item.label, "Reasoning effort");
    assert_eq!(item.value, "future-tier");
    assert_eq!(snapshot().option_count(SettingId::Effort), 0);
    let kept = SettingsSnapshot {
        effort: "high".to_owned(),
        ..snapshot()
    };
    assert_eq!(kept.option_count(SettingId::Effort), 1);
    assert_eq!(
        kept.cycle_change(SettingId::Effort, 1).unwrap().value,
        "default"
    );
}

#[test]
fn categories_cycle_in_both_directions() {
    assert_eq!(SettingCategory::All.cycled(1), SettingCategory::Interface);
    assert_eq!(SettingCategory::All.cycled(-1), SettingCategory::Advanced);
    assert_eq!(SettingCategory::Advanced.cycled(1), SettingCategory::All);
}
