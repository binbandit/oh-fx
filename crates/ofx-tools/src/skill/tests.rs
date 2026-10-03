use std::fs;
use std::path::PathBuf;

use ofx_contract::{PathAccess, ToolCallId, ToolContext, ToolResultStatus};
use ofx_skills::{
    InvalidMetadataCause, RootSpec, Skill, SkillDiagnostic, SkillDiagnosticCause,
    SkillDiagnosticScope, SkillSource, SymlinkAuthorities, build_skill_prompt,
};
use tempfile::TempDir;

use super::*;

const MANAGED_POLICY: RootPolicy = RootPolicy {
    workspace_roots: &[],
    managed_root_source: Some(SkillSource::GlobalOhFx),
    global_roots: &[],
};
const SHARED_ROOTS: [RootSpec; 1] = [RootSpec {
    source: SkillSource::WorkspaceShared,
    path: "skills",
}];
const SHARED_POLICY: RootPolicy = RootPolicy {
    workspace_roots: &SHARED_ROOTS,
    managed_root_source: Some(SkillSource::GlobalOhFx),
    global_roots: &[],
};

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        Self { _temp: temp, root }
    }

    fn path(&self, sub_path: &str) -> PathBuf {
        self.root.join(sub_path)
    }

    fn write(&self, sub_path: &str, content: &str) {
        let path = self.path(sub_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn discovery(&self, managed: &str) -> SkillDiscoveryContext {
        fs::create_dir_all(self.path(managed)).unwrap();
        SkillDiscoveryContext {
            workspace_root: Some(self.root.clone()),
            home: None,
            managed_root: self.path(managed),
            symlink_authorities: SymlinkAuthorities::default(),
        }
    }
}

fn skill(name: &str, path: PathBuf) -> Skill {
    Skill {
        name: name.to_owned(),
        description: String::new(),
        path,
        source: SkillSource::GlobalOhFx,
        read_authority: None,
    }
}

fn tool(discovery: SkillDiscoveryContext, policy: RootPolicy, locations: Locations) -> SkillTool {
    let tool = SkillTool::new(discovery, policy, ContextLimits::default());
    tool.advertise(locations);
    tool
}

fn arguments(location: &str) -> String {
    format!(r#"{{"location":{}}}"#, json_string(location))
}

fn json_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn refusal(tool: &SkillTool, arguments: &str) -> Option<ToolOutput> {
    tool.prepare(arguments).unwrap().refusal().cloned()
}

fn run(tool: &SkillTool, arguments: &str) -> (CallDescription, ToolOutput) {
    run_with(tool, arguments, CancellationToken::new())
}

fn run_with(
    tool: &SkillTool,
    arguments: &str,
    cancellation: CancellationToken,
) -> (CallDescription, ToolOutput) {
    let prepared = tool.prepare(arguments).unwrap();
    let description = prepared.describe();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let context = ToolContext::new(
        ToolCallId::new("call-1"),
        cancellation,
        PathAccess::WorkspaceOnly,
    );
    (description, runtime.block_on(prepared.execute(context)))
}

fn decode_failure(arguments: &str) -> String {
    SkillArgs::decode(arguments).unwrap_err().content
}

#[test]
fn skill_tool_keeps_the_upstream_schema_and_description() {
    let tool = tool(
        Fixture::new().discovery("managed"),
        MANAGED_POLICY,
        Locations::default(),
    );
    let spec = tool.spec();
    assert_eq!(spec.name, "skill");
    assert_eq!(
        spec.description,
        "Load an installed skill or one required relative text resource completely. Copy the exact advertised location. Resolve paths mentioned in skill instructions from the selected skill directory, not the workspace. Read referenced text with the same location and its relative resource path. When to use: the user explicitly invokes a listed skill or the task clearly matches one. When NOT to use: installing a missing skill."
    );
    assert_eq!(
        spec.input_schema,
        r#"{"type":"object","properties":{"location":{"type":"string","description":"The exact advertised location of the selected skill."},"resource":{"type":"string","description":"Optional relative text resource within the selected skill. Omit or pass an empty string to read SKILL.md."}},"additionalProperties":false,"required":["location"]}"#
    );
}

#[test]
fn skill_tool_accepts_an_exact_location_without_a_guessed_name() {
    assert_eq!(
        SkillArgs::decode(r#"{"location":"/installed/workflow"}"#).unwrap(),
        SkillArgs {
            name: None,
            location: Some("/installed/workflow".to_owned()),
            resource: None,
            offset: 0,
        }
    );
}

#[test]
fn skill_tool_decodes_only_valid_argument_shapes() {
    for (arguments, expected) in [
        ("{", "skill arguments must be valid JSON"),
        ("[]", "skill arguments must be an object"),
        ("{}", "skill requires an advertised location"),
        (r#"{"name":1}"#, "skill field \"name\" must be a string"),
        (
            r#"{"name":"workflow","location":1}"#,
            "skill field \"location\" must be a string",
        ),
        (
            r#"{"location":"/installed/workflow","resource":null}"#,
            "skill field \"resource\" must be a string",
        ),
        (
            r#"{"location":"/installed/workflow","resource":1}"#,
            "skill field \"resource\" must be a string",
        ),
        (
            r#"{"location":"/installed/workflow","offset":4}"#,
            "skill offset requires the legacy named resource form",
        ),
        (
            r#"{"name":"workflow","offset":-1}"#,
            "skill field \"offset\" must be a non-negative integer",
        ),
        (
            r#"{"name":"workflow","offset":1.5}"#,
            "skill field \"offset\" must be a non-negative integer",
        ),
    ] {
        assert_eq!(decode_failure(arguments), expected, "{arguments}");
    }
    assert_eq!(
        SkillArgs::decode(r#"{"name":"workflow","location":"/tmp/skills/workflow","offset":7}"#)
            .unwrap(),
        SkillArgs {
            name: Some("workflow".to_owned()),
            location: Some("/tmp/skills/workflow".to_owned()),
            resource: None,
            offset: 7,
        }
    );
}

#[test]
fn skill_presentation_distinguishes_the_initial_document_from_resource_reads() {
    for (arguments, resolved, expected) in [
        (r#"{"name":"workflow"}"#, None, "Loading skill workflow"),
        (
            r#"{"location":"/installed/workflow","resource":""}"#,
            None,
            "Loading skill skill",
        ),
        (
            r#"{"name":"workflow","resource":"SKILL.md","offset":0}"#,
            None,
            "Loading skill workflow",
        ),
        (
            r#"{"name":"workflow","resource":"references/contract-design.md"}"#,
            None,
            "Reading skill resource references/contract-design.md",
        ),
        (
            r#"{"name":"workflow","offset":128}"#,
            None,
            "Reading skill resource SKILL.md",
        ),
        (
            r#"{"name":"workflow","resource":"","offset":128}"#,
            None,
            "Reading skill resource SKILL.md",
        ),
        (
            r#"{"name":"workflow","resource":1,"offset":128}"#,
            None,
            "Loading skill workflow",
        ),
        (
            r#"{"location":"skill:0000000000000001:0/different-directory"}"#,
            Some("workflow"),
            "Loading skill workflow",
        ),
        (
            r#"{"location":"/skills/different-directory","resource":"SKILL.md"}"#,
            Some("workflow"),
            "Loading skill workflow",
        ),
        (
            r#"{"location":"/skills/different-directory","resource":"references/rules.md"}"#,
            Some("workflow"),
            "Reading skill resource references/rules.md",
        ),
        ("[]", None, "Working: skill"),
    ] {
        let label = label(arguments, resolved);
        assert_eq!(
            format_plain_action(TOOL_NAME, label.as_ref()),
            expected,
            "{arguments}"
        );
        if let Some(label) = label {
            let completed = if label.active == "Loading skill" {
                "Loaded skill"
            } else {
                "Read skill resource"
            };
            assert_eq!(label.completed, completed, "{arguments}");
        }
    }
}

#[test]
fn skill_tool_does_not_rebind_an_advertised_location_to_a_renamed_skill() {
    let fixture = Fixture::new();
    fixture.write(
        "skills/workflow/SKILL.md",
        "---\nname: changed\ndescription: changed identity\n---\nWRONG_IDENTITY_BODY\n",
    );
    let locations = Locations {
        namespace: 1,
        roots: vec![fixture.path("skills")],
        skills: vec![skill("original", fixture.path("skills/workflow"))],
        diagnostics: Vec::new(),
    };
    let tool = tool(fixture.discovery("skills"), MANAGED_POLICY, locations);
    let (description, output) = run(&tool, &arguments("skill:0000000000000001:0/workflow"));
    assert_eq!(description.title, "Loading skill original");
    assert_eq!(output.status, ToolResultStatus::Failure);
    assert!(!output.content.contains("WRONG_IDENTITY_BODY"));
}

#[test]
fn skill_preparation_binds_retained_aliases_and_canonical_paths_before_reading() {
    let fixture = Fixture::new();
    fixture.write(
        "skills/workflow/SKILL.md",
        "---\nname: renamed\n---\nREPLACEMENT BODY\n",
    );
    let path = fixture.path("skills/workflow");
    let locations = Locations {
        namespace: 7,
        roots: vec![fixture.path("skills")],
        skills: vec![skill("original", path.clone())],
        diagnostics: vec![SkillDiagnostic {
            path: PathBuf::from("/malformed"),
            source: SkillSource::GlobalOhFx,
            scope: SkillDiagnosticScope::Candidate,
            cause: SkillDiagnosticCause::InvalidMetadata(InvalidMetadataCause::MissingName),
        }],
    };
    let tool = tool(fixture.discovery("skills"), MANAGED_POLICY, locations);
    for location in ["skill:0000000000000007:0/workflow", path.to_str().unwrap()] {
        let (description, output) = run(&tool, &arguments(location));
        assert_eq!(description.title, "Loading skill original", "{location}");
        assert_eq!(output.status, ToolResultStatus::Failure);
        assert!(!output.content.contains("REPLACEMENT BODY"));
        assert!(
            output
                .content
                .starts_with("<skill_discovery_warning details=\"context_notice\" />\n")
        );
        assert_eq!(
            output.context_notices,
            [
                "skill discovery warning: candidate \"/malformed\" was skipped because its metadata is invalid (missing_name); use one safe name and an optional inline description or a >, >-, or | block, then reload skills"
            ]
        );
    }
    assert_eq!(
        refusal(&tool, &arguments("skill:0000000000000006:0/workflow")),
        Some(ToolOutput::failure(
            "skill failed: StaleSkillLocation. Refresh available skills and retry with an exact advertised location."
        ))
    );
    let mismatch = refusal(
        &tool,
        r#"{"name":"renamed","location":"skill:0000000000000007:0/workflow"}"#,
    )
    .unwrap();
    assert!(mismatch.content.contains("does not match"));
}

#[test]
fn skill_preparation_discovers_canonical_locations_added_after_the_catalog() {
    let fixture = Fixture::new();
    fixture.write(
        "managed/new-directory/SKILL.md",
        "---\nname: newly-installed-name\n---\nNEW INSTRUCTIONS\n",
    );
    let path = fixture.path("managed/new-directory");
    let empty_catalog = Locations {
        namespace: 3,
        ..Locations::default()
    };
    let tool = tool(fixture.discovery("managed"), MANAGED_POLICY, empty_catalog);
    let (description, output) = run(&tool, &arguments(path.to_str().unwrap()));
    assert_eq!(description.title, "Loading skill newly-installed-name");
    assert_eq!(description.effect, ToolEffect::ReadOnly);
    assert_eq!(output.status, ToolResultStatus::Success);
    assert!(output.content.contains("NEW INSTRUCTIONS"));
    assert!(output.content.contains("complete=\"true\""));
}

#[test]
fn skill_tool_loads_whole_content_and_resources_from_an_advertised_catalog_location() {
    let fixture = Fixture::new();
    fixture.write(
        "skills/review/SKILL.md",
        "---\nname: review\ndescription: Review changes\n---\nREVIEW BODY\n",
    );
    fixture.write("skills/review/references/checklist.md", "CHECKLIST\n");
    let discovery = fixture.discovery("managed");
    let found = discovery.load_visible_skills(&SHARED_POLICY);
    let catalog = build_skill_prompt(
        &found.skills,
        &found.diagnostics,
        &ContextLimits::default(),
        None,
    );
    let location = format!("skill:{:016x}:0/review", catalog.locations.namespace);
    assert!(catalog.text.contains(&location));
    let tool = tool(discovery, SHARED_POLICY, catalog.locations);

    let (_, output) = run(&tool, &arguments(&location));
    assert_eq!(
        output,
        ToolOutput::success(format!(
            "<skill_content name=\"review\" location=\"{}\" resource=\"SKILL.md\" complete=\"true\">\n---\nname: review\ndescription: Review changes\n---\nREVIEW BODY\n\n</skill_content>",
            fixture.path("skills/review").display()
        ))
    );
    let reference = format!(r#"{{"location":"{location}","resource":"references/checklist.md"}}"#);
    let (description, output) = run(&tool, &reference);
    assert_eq!(
        description.title,
        "Reading skill resource references/checklist.md"
    );
    assert!(
        output
            .content
            .contains("resource=\"references/checklist.md\" complete=\"true\">\nCHECKLIST\n")
    );
    let named = format!(r#"{{"name":"review","location":"{location}","offset":4}}"#);
    let (_, output) = run(&tool, &named);
    assert_eq!(
        output,
        ToolOutput::success(
            "<skill_content name=\"review\" resource=\"SKILL.md\" offset=\"4\" next_offset=\"61\">\nname: review\ndescription: Review changes\n---\nREVIEW BODY\n\n</skill_content>"
        )
    );
    let missing = format!(r#"{{"location":"{location}","resource":"../escape"}}"#);
    assert_eq!(
        run(&tool, &missing).1,
        ToolOutput::failure("skill failed: InvalidSkillResourcePath")
    );
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        run_with(&tool, &arguments(&location), cancellation).1,
        ToolOutput::failure("skill failed: Cancelled")
    );
}

#[test]
fn a_newly_advertised_catalog_replaces_the_locations_the_tool_resolves() {
    let fixture = Fixture::new();
    fixture.write(
        "skills/review/SKILL.md",
        "---\nname: review\n---\nREVIEW BODY\n",
    );
    let discovery = fixture.discovery("managed");
    let tool = SkillTool::new(discovery.clone(), SHARED_POLICY, ContextLimits::default());
    let found = discovery.load_visible_skills(&SHARED_POLICY);
    let catalog = build_skill_prompt(
        &found.skills,
        &found.diagnostics,
        &ContextLimits::default(),
        None,
    );
    let location = format!("skill:{:016x}:0/review", catalog.locations.namespace);
    let stale = refusal(&tool, &arguments(&location)).unwrap();
    assert!(stale.content.contains("StaleSkillLocation"), "{stale:?}");
    tool.advertise(catalog.locations);
    let (_, output) = run(&tool, &arguments(&location));
    assert_eq!(output.status, ToolResultStatus::Success);
    assert!(output.content.contains("REVIEW BODY"));
    assert!(output.context_notices.is_empty());
}

#[test]
fn skill_tool_refuses_undecodable_arguments_before_any_load() {
    let tool = tool(
        Fixture::new().discovery("managed"),
        MANAGED_POLICY,
        Locations::default(),
    );
    let prepared = tool.prepare("{}").unwrap();
    assert_eq!(prepared.describe().effect, ToolEffect::None);
    assert_eq!(
        prepared.refusal(),
        Some(&ToolOutput::failure(
            "skill requires an advertised location"
        ))
    );
    let missing = refusal(&tool, r#"{"name":"absent"}"#).unwrap();
    assert!(missing.content.contains("absent"), "{missing:?}");
}

#[test]
fn skill_tool_refuses_an_unadvertised_catalog_location_without_rediscovery() {
    let fixture = Fixture::new();
    fixture.write("skills/late/SKILL.md", "---\nname: late\n---\nLATE BODY\n");
    let locations = Locations {
        namespace: 9,
        roots: vec![fixture.path("skills")],
        ..Locations::default()
    };
    let tool = tool(fixture.discovery("skills"), MANAGED_POLICY, locations);
    let refused = refusal(&tool, &arguments("skill:0000000000000009:0/late")).unwrap();
    assert!(
        refused
            .content
            .contains("was not found at advertised location"),
        "{refused:?}"
    );
    assert!(!refused.content.contains("LATE BODY"));
}
