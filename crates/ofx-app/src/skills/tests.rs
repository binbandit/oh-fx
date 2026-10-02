use std::os::unix::fs::symlink;

use ofx_config::{ContextLimitOverride, ContextLimitValue};
use ofx_contract::{Tool, ToolCallId, ToolContext, ToolResultStatus};
use ofx_skills::{SkillDiagnosticCause, SkillDiagnosticScope};
use tempfile::TempDir;

use super::*;

struct Fixture {
    _temp: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    paths: ProfilePaths,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let home = fs::canonicalize(temp.path()).unwrap();
        let workspace = home.join("code/app");
        fs::create_dir_all(&workspace).unwrap();
        let paths = ProfilePaths {
            config: home.join(".config/oh-fx"),
            data: home.join(".local/share/oh-fx"),
            state: home.join(".local/state/oh-fx"),
            cache: home.join(".cache/oh-fx"),
        };
        Self {
            _temp: temp,
            home,
            workspace,
            paths,
        }
    }

    fn skill(&self, directory: &str, name: &str) {
        let path = self.home.join(directory).join("SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!("---\nname: {name}\ndescription: {name} workflow\n---\n{name} instructions\n"),
        )
        .unwrap();
    }

    fn settings(&self, json: &str) -> Settings {
        fs::create_dir_all(&self.paths.config).unwrap();
        fs::write(self.paths.config.join("settings.json"), json).unwrap();
        Settings::load(&self.paths, &self.workspace).unwrap()
    }

    fn load_with(&self, settings: &Settings, limits: &ContextLimits) -> HostSkills {
        HostSkills::load(
            &self.workspace,
            Some(self.home.as_os_str()),
            Some(&self.paths),
            settings,
            limits,
        )
    }

    fn load(&self) -> HostSkills {
        self.load_with(&Settings::default(), &ContextLimits::default())
    }
}

fn names(skills: &HostSkills) -> Vec<(String, SkillSource)> {
    skills
        .shared
        .snapshot()
        .skills
        .iter()
        .map(|skill| (skill.name.clone(), skill.source))
        .collect()
}

fn prepare(skills: &HostSkills, prompt: &str) -> SkillContext {
    skills
        .shared
        .prepare(prompt, None, &CancellationToken::new())
        .unwrap()
}

fn load_skill(tool: &SkillTool, location: &str) -> ofx_contract::ToolOutput {
    let prepared = tool
        .prepare(&format!(r#"{{"location":"{location}"}}"#))
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(prepared.execute(ToolContext::new(
        ToolCallId::new("call-1"),
        CancellationToken::new(),
        ofx_contract::PathAccess::WorkspaceOnly,
    )))
}

#[test]
fn workspace_roots_scan_the_product_root_before_compatibility_roots() {
    let workspace: Vec<(&str, SkillSource)> = ROOT_POLICY
        .workspace_roots
        .iter()
        .map(|root| (root.path, root.source))
        .collect();
    assert_eq!(
        workspace,
        [
            (".oh-fx/skills", SkillSource::WorkspaceOhFx),
            ("skills", SkillSource::WorkspaceShared),
            (".opencode/skills", SkillSource::WorkspaceOpencode),
            (".codex/skills", SkillSource::WorkspaceCodex),
            (".claude/skills", SkillSource::WorkspaceClaude),
            (".agents/skills", SkillSource::WorkspaceAgents),
            (".claw/skills", SkillSource::WorkspaceClaw),
        ]
    );
    assert_eq!(
        ROOT_POLICY.managed_root_source,
        Some(SkillSource::GlobalOhFx)
    );
    let global: Vec<(&str, SkillSource)> = ROOT_POLICY
        .global_roots
        .iter()
        .map(|root| (root.path, root.source))
        .collect();
    assert_eq!(
        global,
        [
            (".config/opencode/skills", SkillSource::GlobalOpencode),
            (".codex/skills", SkillSource::GlobalCodex),
            (".claude/skills", SkillSource::GlobalClaude),
            (".agents/skills", SkillSource::GlobalAgents),
            (".claw/skills", SkillSource::GlobalClaw),
        ]
    );
}

#[test]
fn discovery_scans_the_workspace_and_its_ancestors_then_the_managed_and_home_roots() {
    let fixture = Fixture::new();
    fixture.skill("code/app/.oh-fx/skills/alpha", "alpha");
    fixture.skill("code/app/skills/beta", "beta");
    fixture.skill("code/.claude/skills/gamma", "gamma");
    fixture.skill(".claude/skills/delta", "delta");
    fixture.skill(".config/oh-fx/skills/epsilon", "epsilon");
    fixture.skill(".config/opencode/skills/zeta", "zeta");
    fixture.skill(".fx/skills/upstream", "upstream");
    let skills = fixture.load();
    assert_eq!(
        names(&skills),
        [
            ("alpha".to_owned(), SkillSource::WorkspaceOhFx),
            ("beta".to_owned(), SkillSource::WorkspaceShared),
            ("gamma".to_owned(), SkillSource::WorkspaceClaude),
            ("epsilon".to_owned(), SkillSource::GlobalOhFx),
            ("zeta".to_owned(), SkillSource::GlobalOpencode),
            ("delta".to_owned(), SkillSource::GlobalClaude),
        ]
    );
    assert!(skills.shared.snapshot().diagnostics.is_empty());
}

#[test]
fn a_workspace_and_home_reached_through_a_symlink_scan_up_to_home_without_warnings() {
    let fixture = Fixture::new();
    symlink(".", fixture.home.join("alias")).unwrap();
    fixture.skill("code/app/skills/local", "local");
    fixture.skill("code/.agents/skills/between", "between");
    fixture.skill("skills/home-shared", "home-shared");
    fixture.skill(".claude/skills/global", "global");
    let alias = fixture.home.join("alias");
    let skills = HostSkills::load(
        &alias.join("code/app"),
        Some(alias.as_os_str()),
        Some(&fixture.paths),
        &Settings::default(),
        &ContextLimits::default(),
    );
    assert_eq!(
        names(&skills),
        [
            ("local".to_owned(), SkillSource::WorkspaceShared),
            ("between".to_owned(), SkillSource::WorkspaceAgents),
            ("global".to_owned(), SkillSource::GlobalClaude),
        ]
    );
    let found = skills.shared.snapshot();
    assert!(found.diagnostics.is_empty(), "{:?}", found.diagnostics);
    assert_eq!(found.skills[0].path, fixture.workspace.join("skills/local"));
}

#[test]
fn without_home_no_skills_are_discovered_or_loaded() {
    let fixture = Fixture::new();
    fixture.skill("code/app/.oh-fx/skills/alpha", "alpha");
    let skills = HostSkills::load(
        &fixture.workspace,
        None,
        Some(&fixture.paths),
        &Settings::default(),
        &ContextLimits::default(),
    );
    assert!(names(&skills).is_empty());
    assert!(!skills.uses_context_window());
    assert_eq!(prepare(&skills, "$alpha"), SkillContext::default());
    let path = fixture.workspace.join(".oh-fx/skills/alpha");
    let output = load_skill(&skills.tool(), path.to_str().unwrap());
    assert_eq!(output.status, ToolResultStatus::Failure);
}

#[test]
fn a_turn_gets_the_catalog_and_the_skills_its_prompt_names() {
    let fixture = Fixture::new();
    fixture.skill("code/app/.oh-fx/skills/alpha", "alpha");
    fixture.skill(".config/oh-fx/skills/beta", "beta");
    let skills = fixture.load();
    let context = prepare(&skills, "please run $alpha now");
    assert!(context.catalog.starts_with(
        "Skills provide task instructions. Use named skills and clearly matching skills before substantive work.\n"
    ));
    let alpha = fixture.workspace.join(".oh-fx/skills/alpha");
    assert!(
        context
            .catalog
            .contains(&format!("Root 0: {}\n", alpha.parent().unwrap().display()))
    );
    let location = context
        .catalog
        .lines()
        .find_map(|line| line.strip_prefix("- alpha: alpha workflow (location: "))
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap()
        .to_owned();
    assert!(location.starts_with("skill:"), "{location}");
    assert!(
        context
            .catalog
            .contains("- beta: beta workflow (location: skill:")
    );
    assert!(context.explicit.starts_with(
        "Explicitly invoked skill content for this query:\nUse every successfully loaded skill for this query."
    ));
    assert!(context.explicit.contains(&format!(
        "<skill_content name=\"alpha\" location=\"{}\" resource=\"SKILL.md\" complete=\"true\">\n",
        alpha.display()
    )));
    assert!(!context.explicit.contains("beta instructions"));
    assert!(context.context_notices.is_empty());
    assert_eq!(
        context.load_notice,
        Some(Notice::new(
            NoticeTone::Neutral,
            "",
            "1 requested skill loaded\n\u{2514} Loaded skill alpha"
        ))
    );
    let output = load_skill(&skills.tool(), &location);
    assert_eq!(output.status, ToolResultStatus::Success);
    assert!(output.content.contains("alpha instructions"));
    let plain = prepare(&skills, "summarize the repository");
    assert_eq!(plain.catalog, context.catalog);
    assert!(plain.explicit.is_empty());
    assert_eq!(plain.load_notice, None);
}

#[test]
fn skipped_candidates_warn_in_the_catalog_and_in_a_context_notice() {
    let fixture = Fixture::new();
    let broken = fixture.home.join(".config/oh-fx/skills/broken");
    fs::create_dir_all(&broken).unwrap();
    fs::write(broken.join("SKILL.md"), "---\ndescription: nameless\n---\n").unwrap();
    let skills = fixture.load();
    let found = skills.shared.snapshot();
    assert_eq!(found.diagnostics.len(), 1);
    assert_eq!(found.diagnostics[0].scope, SkillDiagnosticScope::Candidate);
    assert!(matches!(
        found.diagnostics[0].cause,
        SkillDiagnosticCause::InvalidMetadata(_)
    ));
    let context = prepare(&skills, "$broken");
    assert_eq!(
        context.catalog,
        "<skill_discovery_warning skipped_candidate_count=\"1\" incomplete_root_count=\"0\" missing_from_incomplete_roots=\"0\" />\n"
    );
    assert_eq!(context.context_notices.len(), 1);
    assert!(
        context.context_notices[0].starts_with(&format!(
            "skill discovery warning: candidate \"{}\" was skipped because its metadata is invalid",
            broken.display()
        )),
        "{:?}",
        context.context_notices
    );
}

#[test]
fn a_refresh_finds_skills_added_since_the_last_scan() {
    let fixture = Fixture::new();
    let skills = fixture.load();
    assert!(!skills.uses_context_window());
    assert_eq!(prepare(&skills, "$late"), SkillContext::default());
    fixture.skill("code/app/skills/late", "late");
    assert!(names(&skills).is_empty());
    skills.refresh();
    assert_eq!(
        names(&skills),
        [("late".to_owned(), SkillSource::WorkspaceShared)]
    );
    assert!(skills.uses_context_window());
    assert!(
        prepare(&skills, "$late")
            .explicit
            .contains("late instructions")
    );
}

#[test]
fn a_configured_catalog_budget_does_not_need_the_context_window() {
    let fixture = Fixture::new();
    fixture.skill("code/app/skills/alpha", "alpha");
    let mut limits = ContextLimits::default();
    limits.apply_command_line(&[ContextLimitOverride {
        name: ContextLimitName::SkillCatalogBytes,
        value: ContextLimitValue::Bytes(64 * 1024),
    }]);
    let skills = fixture.load_with(&Settings::default(), &limits);
    assert!(!skills.uses_context_window());
    assert!(fixture.load().uses_context_window());
}

#[test]
fn configured_symlink_authorities_admit_linked_workspace_skills() {
    let fixture = Fixture::new();
    fixture.skill("shared/linked", "linked");
    let root = fixture.workspace.join(".oh-fx/skills");
    fs::create_dir_all(&root).unwrap();
    symlink(fixture.home.join("shared/linked"), root.join("linked")).unwrap();
    let unauthorized = fixture.load();
    assert!(names(&unauthorized).is_empty());
    assert_eq!(
        unauthorized.shared.snapshot().diagnostics[0].cause,
        SkillDiagnosticCause::LinkedCandidateUnavailable
    );
    let settings = fixture.settings(&format!(
        r#"{{"skill_symlink_authorities":["{}"]}}"#,
        fixture.home.join("shared").display()
    ));
    let authorized = fixture.load_with(&settings, &ContextLimits::default());
    assert_eq!(
        names(&authorized),
        [("linked".to_owned(), SkillSource::WorkspaceOhFx)]
    );
}
