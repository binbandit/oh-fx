use super::*;

fn model(id: &str, released: i64, tool_use: bool) -> GatewayModel {
    GatewayModel {
        id: id.to_owned(),
        released,
        tool_use,
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
