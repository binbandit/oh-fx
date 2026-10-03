use super::*;
use crate::SkillSource;
use ofx_text::QueryTooLong;
fn skill(name: &str, description: &str, path: &str) -> Skill {
    Skill {
        name: name.to_owned(),
        description: description.to_owned(),
        path: path.into(),
        source: SkillSource::WorkspaceOhFx,
        read_authority: None,
    }
}
fn query(raw: &str) -> PreparedQuery {
    PreparedQuery::prepare(raw.to_owned()).unwrap()
}
fn names(result: &SkillSearchResult) -> Vec<String> {
    let items: Vec<serde_json::Value> =
        serde_json::from_str(&format!("[{}]", result.items_json)).unwrap();
    items
        .iter()
        .map(|v| v["name"].as_str().unwrap().to_owned())
        .collect()
}
#[test]
fn prepared_query_enforces_raw_byte_limit_and_accepts_whitespace() {
    assert_eq!(
        PreparedQuery::prepare("é".repeat(2049)).unwrap_err(),
        QueryTooLong
    );
    assert!(PreparedQuery::prepare("é".repeat(2048)).is_ok());
    assert!(PreparedQuery::prepare(" ".to_owned()).is_ok());
}
#[test]
fn metadata_ranking_requires_clear_intent_and_preserves_exact_identity() {
    let skills = [
        skill(
            "general",
            "Review ordinary text",
            "/skills/general/SKILL.md",
        ),
        skill(
            "fx-review",
            "Review fx runtime changes",
            "/skills/fx-review/SKILL.md",
        ),
    ];
    let result = search_skills(&query("review fx runtime public"), &skills, 1024, 4096).unwrap();
    assert_eq!(names(&result), ["fx-review"]);
    assert_eq!(result.total_matches, 1);
    let result = search_skills(&query("general"), &skills, 1024, 4096).unwrap();
    assert_eq!(names(&result), ["general"]);
}
#[test]
fn intent_rejects_corpus_wide_procedural_and_short_weak_tokens() {
    let skills = [
        skill("query-docs", "Query and list documentation", "/a"),
        skill("resolve-library-id", "Query and list documentation", "/b"),
    ];
    assert_eq!(
        search_skills(
            &query("query list production monitors"),
            &skills,
            1024,
            4096
        )
        .unwrap()
        .count,
        0
    );
    let skills: Vec<_> = (0..32)
        .map(|i| {
            skill(
                if i == 0 {
                    "cloud-helper"
                } else {
                    "generic-helper"
                },
                if i == 0 {
                    "AWS deployment guidance"
                } else {
                    "General workflow guidance"
                },
                &format!("/{i}"),
            )
        })
        .collect();
    assert_eq!(
        names(&search_skills(&query("aws deployment"), &skills, 1024, 4096).unwrap()),
        ["cloud-helper"]
    );
}
#[test]
fn inventory_limits_five_then_drops_whole_items_for_budget() {
    let skills: Vec<_> = (0..7)
        .map(|i| skill(&format!("skill-{i}"), "description", &format!("/{i}")))
        .collect();
    let result = search_skills(&query(""), &skills, 1024, 4096).unwrap();
    assert_eq!(result.count, 5);
    assert_eq!(result.total_matches, 7);
    assert_eq!(
        search_skills(&query(""), &skills, 1024, 1).unwrap_err(),
        SkillSearchError::ResultLimitTooSmall
    );
}
#[test]
fn result_locations_and_secret_shaped_descriptions_remain_verbatim() {
    let skills = [skill(
        "safe",
        "API_KEY=description-secret-value",
        "/skills/TOKEN=path-secret-value",
    )];
    let result = search_skills(&query("safe"), &skills, 1024, 4096).unwrap();
    assert!(
        result
            .items_json
            .contains("API_KEY=description-secret-value")
    );
    assert!(
        result
            .items_json
            .contains("/skills/TOKEN=path-secret-value")
    );
}

#[test]
fn retained_prefix_budget_includes_the_source_cursor_envelope() {
    let skills: Vec<_> = (0..7)
        .map(|i| skill(&format!("skill-{i}"), "description", &format!("/{i}")))
        .collect();
    let result = search_skills(&query(""), &skills, 1024, 4096).unwrap();
    let exact = format!(
        "{{\"skills\":[{}],\"count\":5,\"total_matches\":7,\"more_available\":true,\"next_cursor\":\"c1:s:2e2e1cae8e5ca8b4:2b7dc6aa7abcd396:5\"}}",
        result.items_json
    );
    assert_eq!(
        search_skills(&query(""), &skills, 1024, exact.len())
            .unwrap()
            .count,
        5
    );
    assert_eq!(
        search_skills(&query(""), &skills, 1024, exact.len() - 1)
            .unwrap()
            .count,
        4
    );
}

#[test]
fn description_clips_utf8_while_unsafe_identities_are_not_returned() {
    let skills = [
        skill("nul\0name", "secret", "/unsafe"),
        skill("safe", "é日x", "/safe"),
    ];
    let result = search_skills(&query(""), &skills, 4, 4096).unwrap();
    assert_eq!(names(&result), ["safe"]);
    let items: Vec<serde_json::Value> =
        serde_json::from_str(&format!("[{}]", result.items_json)).unwrap();
    assert_eq!(items[0]["description"], "é");
    assert_eq!(result.total_matches, 1);
}
