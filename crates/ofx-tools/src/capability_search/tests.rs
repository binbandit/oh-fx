use ofx_contract::{PathAccess, ToolCallId, ToolResultStatus};
use ofx_skills::SymlinkAuthorities;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::*;

fn tool(max_bytes: usize) -> CapabilitySearch {
    CapabilitySearch::new(
        SkillDiscoveryContext {
            workspace_root: None,
            home: None,
            managed_root: "/nonexistent".into(),
            symlink_authorities: SymlinkAuthorities::default(),
        },
        RootPolicy {
            workspace_roots: &[],
            managed_root_source: None,
            global_roots: &[],
        },
        ContextLimits::default(),
        max_bytes,
    )
}

async fn execute(arguments: &str, max_bytes: usize, cancellation: CancellationToken) -> ToolOutput {
    tool(max_bytes)
        .prepare(arguments)
        .expect("prepare search")
        .execute(ToolContext::new(
            ToolCallId::new("search-1"),
            cancellation,
            PathAccess::WorkspaceOnly,
        ))
        .await
}

#[test]
fn schema_and_description_match_the_pinned_export() {
    let expected: Value =
        serde_json::from_str(include_str!("schema.json")).expect("parse source schema");
    let search = tool(16384);
    assert_eq!(search.spec().name, expected["name"]);
    assert_eq!(search.spec().description, expected["description"]);
    assert_eq!(
        search.spec().input_schema,
        expected["inputSchema"].to_string()
    );
}

#[tokio::test]
async fn decoder_preserves_exact_source_errors() {
    let cases = [
        ("{", "capability_search arguments must be valid JSON"),
        ("[]", "capability_search arguments must be an object"),
        ("{}", "capability_search field \"query\" is required"),
        (
            "{\"query\":null}",
            "capability_search field \"query\" must be a string",
        ),
        (
            "{\"query\":\"\"}",
            "capability_search field \"query\" must not be empty",
        ),
        (
            "{\"query\":\"q\",\"server\":null}",
            "capability_search field \"server\" must be a string",
        ),
        (
            "{\"query\":\"q\",\"server\":\"\"}",
            "capability_search field \"server\" must not be empty",
        ),
    ];
    for (arguments, expected) in cases {
        let result = execute(arguments, 16384, CancellationToken::new()).await;
        assert_eq!(result.status, ToolResultStatus::Failure);
        assert_eq!(result.content, expected);
    }
    let arguments = json!({"query": "é".repeat(2049)}).to_string();
    assert_eq!(
        execute(&arguments, 16384, CancellationToken::new())
            .await
            .content,
        "capability_search query must not exceed 4096 bytes"
    );
}

#[tokio::test]
async fn empty_skill_catalog_preserves_unavailable_mcp_instead_of_no_match() {
    assert_eq!(
        execute(
            r#"{"query":"   ","extra":true}"#,
            16384,
            CancellationToken::new()
        )
        .await
        .content,
        r#"{"skills":[],"mcp_tools":[],"counts":{"skills":0,"mcp_tools":0},"total_matches":{"skills":0,"mcp_tools":0},"mcp_state":"unavailable"}"#
    );
}

#[tokio::test]
async fn combined_budget_failure_has_the_source_domain_and_error() {
    assert_eq!(
        execute(
            r#"{"query":"q","server":"exact"}"#,
            1,
            CancellationToken::new()
        )
        .await
        .content,
        "capability_search combined search failed: CapabilitySearchResultLimitTooSmall"
    );
}

struct Fixture {
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().expect("temporary catalog"),
        }
    }
    fn write(&self, name: &str, description: &str) {
        let root = self.directory.path().join(name);
        std::fs::create_dir_all(&root).expect("create skill");
        std::fs::write(
            root.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nSkill body\n"),
        )
        .expect("write skill");
    }
    fn search(&self, max_bytes: usize) -> CapabilitySearch {
        CapabilitySearch::new(
            SkillDiscoveryContext {
                workspace_root: None,
                home: None,
                managed_root: std::fs::canonicalize(self.directory.path()).expect("canonical root"),
                symlink_authorities: SymlinkAuthorities::default(),
            },
            RootPolicy {
                workspace_roots: &[],
                managed_root_source: Some(ofx_skills::SkillSource::GlobalOhFx),
                global_roots: &[],
            },
            ContextLimits::default(),
            max_bytes,
        )
    }
}

async fn run(search: &CapabilitySearch, arguments: &str) -> ToolOutput {
    search
        .prepare(arguments)
        .expect("prepare search")
        .execute(ToolContext::new(
            ToolCallId::new("search-2"),
            CancellationToken::new(),
            PathAccess::WorkspaceOnly,
        ))
        .await
}

#[tokio::test]
async fn exact_identity_returns_the_loadable_path_and_fresh_catalog_changes() {
    let fixture = Fixture::new();
    fixture.write("mail-helper", "Send email");
    let search = fixture.search(16384);
    let output = run(&search, r#"{"query":"mail-helper"}"#).await;
    assert_eq!(output.status, ToolResultStatus::Success);
    let result: Value = serde_json::from_str(&output.content).expect("valid combined JSON");
    assert_eq!(result["skills"][0]["name"], "mail-helper");
    assert_eq!(
        result["skills"][0]["location"],
        std::fs::canonicalize(fixture.directory.path())
            .expect("root")
            .join("mail-helper")
            .to_str()
            .expect("UTF8 path")
    );
    assert_eq!(result["mcp_state"], "unavailable");
    assert!(result.get("next_cursor").is_none());
    fixture.write("second-helper", "Second skill");
    let fresh: Value =
        serde_json::from_str(&run(&search, r#"{"query":"second-helper"}"#).await.content)
            .expect("fresh JSON");
    assert_eq!(fresh["skills"][0]["name"], "second-helper");
}

#[tokio::test]
async fn exact_server_scope_skips_skill_discovery_and_diagnostics() {
    let fixture = Fixture::new();
    fixture.write("mail-helper", "Send email");
    let broken = fixture.directory.path().join("broken");
    std::fs::create_dir(&broken).expect("broken candidate");
    std::fs::write(
        broken.join("SKILL.md"),
        "---\ndescription: missing name\n---\nBody",
    )
    .expect("broken metadata");
    let output = run(
        &fixture.search(16384),
        r#"{"query":"mail-helper","server":"mail"}"#,
    )
    .await;
    assert_eq!(output.status, ToolResultStatus::Success);
    assert!(output.context_notices.is_empty());
    assert_eq!(
        output.content,
        r#"{"skills":[],"mcp_tools":[],"counts":{"skills":0,"mcp_tools":0},"total_matches":{"skills":0,"mcp_tools":0},"mcp_state":"unavailable"}"#
    );
}

#[tokio::test]
async fn discovery_diagnostics_are_context_notices_not_model_result_fields() {
    let fixture = Fixture::new();
    let broken = fixture.directory.path().join("broken");
    std::fs::create_dir(&broken).expect("candidate");
    std::fs::write(
        broken.join("SKILL.md"),
        "---\ndescription: missing name\n---\nBody",
    )
    .expect("metadata");
    let output = run(&fixture.search(16384), r#"{"query":"anything"}"#).await;
    assert_eq!(output.context_notices.len(), 1);
    assert!(output.context_notices[0].starts_with("skill discovery warning: "));
    assert!(!output.content.contains("warning"));
}

#[tokio::test]
async fn catalog_counts_total_matches_before_retaining_five() {
    let fixture = Fixture::new();
    for name in ["a", "b", "c", "d", "e", "f"] {
        fixture.write(name, "A skill");
    }
    let output = run(&fixture.search(16384), r#"{"query":"a b c d e f"}"#).await;
    let result: Value = serde_json::from_str(&output.content).expect("combined");
    assert_eq!(result["counts"]["skills"], 5);
    assert_eq!(result["total_matches"]["skills"], 6);
    assert_eq!(result["skills"].as_array().expect("skill entries").len(), 5);
}

#[test]
fn presentation_uses_the_query_or_exact_server_without_execution() {
    let search = tool(16384);
    for (arguments, title) in [
        (
            r#"{"query":"send email"}"#,
            "Searching capabilities send email",
        ),
        (
            r#"{"query":"ignored","server":"clerk"}"#,
            "Searching capabilities clerk",
        ),
    ] {
        let call = search.prepare(arguments).expect("prepared");
        let description = call.describe();
        assert_eq!(description.title, title);
        assert_eq!(description.effect, ToolEffect::ReadOnly);
        assert_eq!(description.activity, ToolActivity::Read);
        assert_eq!(description.concurrency, Concurrency::Parallel);
    }
}

#[tokio::test]
async fn unavailable_mcp_still_reserves_half_the_combined_budget() {
    let fixture = Fixture::new();
    fixture.write("mail-helper", &"x".repeat(1000));
    let output = run(&fixture.search(1024), r#"{"query":"mail-helper"}"#).await;
    assert_eq!(output.status, ToolResultStatus::Success);
    let result: Value = serde_json::from_str(&output.content).expect("bounded JSON");
    assert_eq!(result["counts"]["skills"], 0);
    assert_eq!(result["total_matches"]["skills"], 1);
    assert_eq!(result["mcp_state"], "unavailable");
}

#[tokio::test]
async fn description_limit_preserves_complete_utf8_and_domain_budget_error() {
    let fixture = Fixture::new();
    fixture.write("mail-helper", "ééé");
    let mut search = fixture.search(16384);
    let mut limits = ContextLimits::default();
    limits.apply_command_line(&[ofx_config::parse_context_limit_override(
        b"skill_description_bytes=5",
    )
    .expect("limit")]);
    Arc::get_mut(&mut search.context)
        .expect("unique context")
        .limits = limits;
    let result: Value =
        serde_json::from_str(&run(&search, r#"{"query":"mail-helper"}"#).await.content)
            .expect("JSON");
    assert_eq!(result["skills"][0]["description"], "éé");
    let output = run(&fixture.search(1), r#"{"query":"mail-helper"}"#).await;
    assert_eq!(
        output.content,
        "capability_search skill search failed: SkillSearchResultLimitTooSmall"
    );
}

#[test]
fn malformed_object_presentations_keep_source_defaults() {
    for (args, title) in [
        ("{}", "Searching capabilities capabilities"),
        (r#"{"query":null}"#, "Searching capabilities capabilities"),
        (r#"{"query":"q","server":""}"#, "Searching capabilities q"),
        ("[]", "Working: capability_search"),
        ("{", "Working: capability_search"),
    ] {
        assert_eq!(tool(16384).prepare(args).unwrap().describe().title, title);
    }
}

#[tokio::test]
async fn interactive_empty_host_reports_no_match_for_both_scopes() {
    for args in [
        r#"{"query":"absent"}"#,
        r#"{"query":"absent","server":"absent"}"#,
    ] {
        let output = tool(16384)
            .with_interactive_host(true)
            .prepare(args)
            .unwrap()
            .execute(ToolContext::new(
                ToolCallId::new("scope"),
                CancellationToken::new(),
                PathAccess::WorkspaceOnly,
            ))
            .await;
        assert_eq!(
            output.content,
            r#"{"skills":[],"mcp_tools":[],"counts":{"skills":0,"mcp_tools":0},"total_matches":{"skills":0,"mcp_tools":0},"state":"no_match"}"#
        );
    }
}

#[test]
fn saved_searches_describe_themselves_as_their_calls_did() {
    for (arguments, target) in [
        (r#"{"query":"send email"}"#, "send email"),
        (r#"{"query":"ignored","server":"clerk"}"#, "clerk"),
        ("{}", "capabilities"),
    ] {
        let search = tool(16384);
        let saved = search
            .describe_saved(arguments)
            .expect("a saved description");
        assert_eq!(saved, search.prepare(arguments).unwrap().describe());
        let label = saved.label.expect("a label");
        assert_eq!(label.completed, "Searched capabilities");
        assert_eq!(label.target, target);
        assert_eq!(saved.activity, ToolActivity::Read);
    }
}
