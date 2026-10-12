use super::*;

fn model(id: &str, released: i64, tool_use: bool) -> GatewayModel {
    GatewayModel {
        id: id.to_owned(),
        released,
        tool_use,
        ..GatewayModel::default()
    }
}

fn ids(models: &[GatewayModel]) -> Vec<&str> {
    models.iter().map(|model| model.id.as_str()).collect()
}

fn sorted(mut models: Vec<GatewayModel>) -> Vec<GatewayModel> {
    models.sort_by(catalog_order);
    models
}

#[test]
fn catalog_order_prefers_tool_use_then_tier_provider_recency_and_id() {
    let models = sorted(vec![
        model("vendor/plain", 500, false),
        model("openai/gpt-6-preview", 400, true),
        model("openai/o4-mini", 300, true),
        model("xai/grok-3-flash", 200, true),
        model("xai/grok-4.7", 100, true),
        model("openai/gpt-5.5", 3, true),
        model("openai/gpt-5.6-sol", 5, true),
        model("openai/gpt-5.2-codex", 3, true),
        model("vendor/model-2", 9, true),
    ]);
    assert_eq!(
        ids(&models),
        [
            "openai/gpt-5.6-sol",
            "openai/gpt-5.2-codex",
            "openai/gpt-5.5",
            "xai/grok-4.7",
            "vendor/model-2",
            "xai/grok-3-flash",
            "openai/o4-mini",
            "openai/gpt-6-preview",
            "vendor/plain",
        ]
    );
}

#[test]
fn tiers_and_providers_follow_upstream_names_in_any_letter_case() {
    assert_eq!(tier_rank("vendor/Model-BETA"), 4);
    assert_eq!(tier_rank("vendor/model-Lite"), 3);
    assert_eq!(tier_rank("vendor/model-flash"), 2);
    assert_eq!(tier_rank("vendor/model-PRO"), 0);
    assert_eq!(tier_rank("vendor/model"), 1);
    assert_eq!(provider_rank("openai/gpt-5"), 1);
    assert_eq!(provider_rank("alibaba/qwen-3"), 7);
    assert_eq!(provider_rank("moonshotai/kimi-k3"), 8);
}

#[test]
fn picker_families_group_versions_and_named_lines() {
    for (id, family) in [
        ("openai/gpt-5.5-pro", "openai/gpt"),
        ("openai/gpt-5.3-Codex", "openai/gpt-codex"),
        ("deepseek/deepseek-v4", "deepseek/deepseek"),
        ("minimax/minimax-m3", "minimax/minimax"),
        ("xai/grok-build-0.1", "xai/grok-build"),
        ("openai/o3", "openai/o3"),
        ("plain-1", "plain-1"),
        ("vendor/model-", "vendor/model-"),
    ] {
        assert_eq!(family_of(id), family, "{id}");
    }
}

#[test]
fn the_picker_puts_highlights_first_then_each_provider_then_the_whole_catalog() {
    let catalog = sorted(vec![
        model("alibaba/qwen-3.7-max", 130, true),
        model("alibaba/qwen-3.7-plus", 120, true),
        model("openai/gpt-5.5", 90, true),
        model("openai/gpt-5.5-pro", 90, true),
        model("openai/gpt-5.4", 85, true),
        model("openai/gpt-5.4-mini", 85, true),
        model("openai/gpt-5.3-codex", 80, true),
        model("openai/gpt-5.2-codex", 70, true),
        model("openai/gpt-5.2", 70, true),
        model("openai/o3", 110, true),
        model("openai/gpt-oss-120b", 100, true),
        model("xai/grok-build-0.1", 95, true),
        model("google/gemini-3.1-pro", 120, true),
        model("zai/glm-5.2", 100, true),
        model("zai/glm-5.2-fast", 100, true),
        model("zai/glm-5.1", 90, true),
        model("zai/glm-5", 80, true),
        model("deepseek/deepseek-v4", 110, true),
        model("deepseek/deepseek-v4-flash", 110, true),
        model("deepseek/deepseek-v4-pro", 110, true),
        model("minimax/minimax-m3", 140, true),
        model("minimax/minimax-m2.7", 130, true),
    ]);
    assert_eq!(
        ids(&picker(catalog)),
        [
            "openai/gpt-5.5",
            "xai/grok-build-0.1",
            "zai/glm-5.2",
            "deepseek/deepseek-v4-pro",
            "minimax/minimax-m3",
            "alibaba/qwen-3.7-max",
            "alibaba/qwen-3.7-plus",
            "deepseek/deepseek-v4",
            "google/gemini-3.1-pro",
            "minimax/minimax-m2.7",
            "openai/gpt-5.5-pro",
            "openai/gpt-5.3-codex",
            "zai/glm-5.1",
            "openai/o3",
            "openai/gpt-5.4",
            "openai/gpt-5.2",
            "openai/gpt-5.2-codex",
            "openai/gpt-oss-120b",
            "zai/glm-5.2-fast",
            "zai/glm-5",
            "deepseek/deepseek-v4-flash",
            "openai/gpt-5.4-mini",
        ]
    );
}

#[test]
fn the_picker_lists_a_repeated_id_once() {
    let catalog = vec![model("vendor/a-1", 2, true), model("vendor/a-1", 1, true)];
    assert_eq!(ids(&picker(catalog)), ["vendor/a-1"]);
}

#[test]
fn a_featured_deepseek_slot_prefers_pro_then_reasoning_then_plain_then_flash() {
    let catalog = vec![
        model("deepseek/deepseek-v5-flash", 9, true),
        model("deepseek/deepseek-v5", 9, true),
        model("deepseek/deepseek-v5-thinking", 9, true),
    ];
    assert_eq!(picker(catalog)[0].id, "deepseek/deepseek-v5-thinking");
}
