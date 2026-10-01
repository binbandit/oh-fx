use std::fs;
use std::os::unix::fs::symlink;

use tempfile::TempDir;

use super::*;
use crate::skill_contract::InvalidMetadataCause;

const TEST_WORKSPACE_ROOTS: [RootSpec; 3] = [
    RootSpec {
        source: SkillSource::WorkspaceShared,
        path: "skills",
    },
    RootSpec {
        source: SkillSource::WorkspaceCodex,
        path: ".codex/skills",
    },
    RootSpec {
        source: SkillSource::WorkspaceAgents,
        path: ".agents/skills",
    },
];

const TEST_GLOBAL_ROOTS: [RootSpec; 2] = [
    RootSpec {
        source: SkillSource::GlobalCodex,
        path: ".codex/skills",
    },
    RootSpec {
        source: SkillSource::GlobalAgents,
        path: ".agents/skills",
    },
];

const TEST_ROOT_POLICY: RootPolicy = RootPolicy {
    workspace_roots: &TEST_WORKSPACE_ROOTS,
    managed_root_source: Some(SkillSource::GlobalOhFx),
    global_roots: &TEST_GLOBAL_ROOTS,
};

const TEST_MANAGED_ROOT_POLICY: RootPolicy = RootPolicy {
    workspace_roots: &[],
    managed_root_source: Some(SkillSource::GlobalOhFx),
    global_roots: &[],
};

const CUSTOM_ROOTS: [RootSpec; 1] = [RootSpec {
    source: SkillSource::WorkspaceClaw,
    path: "custom-skills",
}];

const ALIAS_WORKSPACE_ROOTS: [RootSpec; 2] = [
    RootSpec {
        source: SkillSource::WorkspaceClaude,
        path: ".claude/skills",
    },
    RootSpec {
        source: SkillSource::WorkspaceAgents,
        path: ".agents/skills",
    },
];

const ALIAS_GLOBAL_ROOTS: [RootSpec; 2] = [
    RootSpec {
        source: SkillSource::GlobalClaude,
        path: ".claude/skills",
    },
    RootSpec {
        source: SkillSource::GlobalAgents,
        path: ".agents/skills",
    },
];

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

    fn write(&self, sub_path: &str, content: impl AsRef<[u8]>) {
        let path = self.path(sub_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn mkdir(&self, sub_path: &str) {
        fs::create_dir_all(self.path(sub_path)).unwrap();
    }

    fn symlink(&self, target: &str, link: &str) {
        let link = self.path(link);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(target, link).unwrap();
    }

    fn real(&self, sub_path: &str) -> PathBuf {
        fs::canonicalize(self.path(sub_path)).unwrap()
    }

    fn home_context(&self, workspace: &str) -> SkillDiscoveryContext {
        self.mkdir("home/.oh-fx/skills");
        SkillDiscoveryContext {
            workspace_root: Some(self.real(workspace)),
            home: Some(self.real("home")),
            managed_root: self.real("home/.oh-fx/skills"),
        }
    }

    fn managed_discovery(&self, managed: &str) -> SkillDiscovery {
        SkillDiscoveryContext {
            workspace_root: None,
            home: None,
            managed_root: self.path(managed),
        }
        .load_visible_skills(&TEST_MANAGED_ROOT_POLICY)
    }
}

fn only_diagnostic(discovery: &SkillDiscovery) -> &SkillDiagnostic {
    assert_eq!(discovery.diagnostics.len(), 1, "{discovery:?}");
    &discovery.diagnostics[0]
}

fn named<'a>(discovery: &'a SkillDiscovery, name: &str) -> Option<&'a Skill> {
    discovery.skills.iter().find(|skill| skill.name == name)
}

#[test]
fn skill_discovery_bounds_near_emergency_valid_metadata() {
    let fixture = Fixture::new();
    let mut content = b"---\nname: ".to_vec();
    content.resize(content.len() + 2 * 1024 * 1024, b'n');
    content.extend_from_slice(b"\n---\nbody\n");
    fixture.write("root/huge/SKILL.md", content);

    let discovery = fixture.managed_discovery("root");
    assert!(discovery.skills.is_empty());
    assert_eq!(
        only_diagnostic(&discovery).cause,
        SkillDiagnosticCause::Oversized
    );
}

#[test]
fn skill_discovery_stops_reading_metadata_after_the_header_bound() {
    let fixture = Fixture::new();
    let mut content = b"---\nname: ".to_vec();
    content.resize(content.len() + 100 * 1024, b'n');
    content.extend_from_slice(b"\n---\nbody\n");
    fixture.write("root/large/SKILL.md", content);

    let discovery = fixture.managed_discovery("root");
    assert!(discovery.skills.is_empty());
    assert_eq!(
        only_diagnostic(&discovery).cause,
        SkillDiagnosticCause::Oversized
    );
}

#[test]
fn skill_discovery_decodes_supported_descriptions_and_isolates_malformed_neighbors() {
    let fixture = Fixture::new();
    fixture.write(
        "root/folded/SKILL.md",
        "---\nname: folded\ndescription: >\n  first line\n  second line\n\n  next paragraph\n---\nbody\n",
    );
    fixture.write(
        "root/literal/SKILL.md",
        "---\nname: literal\ndescription: |\n  first line\n  second line\n---\nbody\n",
    );
    fixture.write(
        "root/inline/SKILL.md",
        "---\nname: inline\ndescription: keeps  internal   spaces\n---\nbody\n",
    );
    fixture.write(
        "root/bad/SKILL.md",
        "---\nname: bad\ndescription: >\n   first\n  smaller indent\n---\nbody\n",
    );

    let discovery = fixture.managed_discovery("root");
    assert_eq!(discovery.skills.len(), 3);
    let description = |name| named(&discovery, name).unwrap().description.as_str();
    assert_eq!(description("inline"), "keeps  internal   spaces");
    assert_eq!(
        description("folded"),
        "first line second line\n\nnext paragraph\n"
    );
    assert_eq!(description("literal"), "first line\nsecond line\n");
    assert_eq!(
        only_diagnostic(&discovery).cause,
        SkillDiagnosticCause::InvalidMetadata(InvalidMetadataCause::UnsupportedMultiline)
    );
}

#[test]
fn load_visible_skills_scans_only_roots_supplied_by_policy() {
    let fixture = Fixture::new();
    fixture.write(
        "home/workspace/app/custom-skills/injected/SKILL.md",
        "---\nname: injected\ndescription: selected by policy\n---\nbody\n",
    );
    fixture.write(
        "home/workspace/app/skills/ignored/SKILL.md",
        "---\nname: ignored\ndescription: not selected\n---\nbody\n",
    );
    fixture.write(
        "home/.oh-fx/skills/ignored-managed/SKILL.md",
        "---\nname: ignored-managed\ndescription: not selected\n---\nbody\n",
    );
    let policy = RootPolicy {
        workspace_roots: &CUSTOM_ROOTS,
        managed_root_source: None,
        global_roots: &[],
    };

    let discovery = fixture
        .home_context("home/workspace/app")
        .load_visible_skills(&policy);
    assert_eq!(discovery.skills.len(), 1);
    assert_eq!(discovery.skills[0].name, "injected");
    assert_eq!(discovery.skills[0].source, SkillSource::WorkspaceClaw);
    assert!(named(&discovery, "ignored").is_none());
    assert!(named(&discovery, "ignored-managed").is_none());
}

#[test]
fn load_visible_skills_preserves_root_distinct_duplicate_skill_names() {
    let fixture = Fixture::new();
    fixture.write(
        "home/workspace/app/.agents/skills/review/SKILL.md",
        "---\nname: review\ndescription: closest\n---\n\nclosest body\n",
    );
    fixture.write(
        "home/workspace/.agents/skills/review/SKILL.md",
        "---\nname: review\ndescription: ancestor\n---\n\nancestor body\n",
    );
    fixture.write(
        "home/.oh-fx/skills/review/SKILL.md",
        "---\nname: review\ndescription: managed\n---\n\nmanaged body\n",
    );
    fixture.write(
        "home/.agents/skills/review/SKILL.md",
        "---\nname: review\ndescription: global compatibility\n---\n\nglobal body\n",
    );

    let discovery = fixture
        .home_context("home/workspace/app")
        .load_visible_skills(&TEST_ROOT_POLICY);
    let found: Vec<(&str, SkillSource)> = discovery
        .skills
        .iter()
        .map(|skill| (skill.description.as_str(), skill.source))
        .collect();
    assert_eq!(
        found,
        [
            ("closest", SkillSource::WorkspaceAgents),
            ("ancestor", SkillSource::WorkspaceAgents),
            ("managed", SkillSource::GlobalOhFx),
            ("global compatibility", SkillSource::GlobalAgents),
        ]
    );
}

#[test]
fn load_visible_skills_deduplicates_symlinked_workspace_and_global_roots_while_preserving_physical_same_name_candidates()
 {
    let fixture = Fixture::new();
    fixture.write(
        "home/workspace/.agents/skills/alpha/SKILL.md",
        "---\nname: review\ndescription: alpha workflow\n---\n\nalpha body\n",
    );
    fixture.write(
        "home/workspace/.agents/skills/beta/SKILL.md",
        "---\nname: review\ndescription: beta workflow\n---\n\nbeta body\n",
    );
    fixture.write(
        "home/.agents/skills/global/SKILL.md",
        "---\nname: review\ndescription: global workflow\n---\n\nglobal body\n",
    );
    fixture.symlink("../.agents/skills", "home/workspace/.claude/skills");
    fixture.symlink("../.agents/skills", "home/.claude/skills");
    let policy = RootPolicy {
        workspace_roots: &ALIAS_WORKSPACE_ROOTS,
        managed_root_source: None,
        global_roots: &ALIAS_GLOBAL_ROOTS,
    };
    let workspace_root = fixture.real("home/workspace");
    let home_root = fixture.real("home");

    let discovery = fixture
        .home_context("home/workspace")
        .load_visible_skills(&policy);
    let found: Vec<(&str, &str, SkillSource)> = discovery
        .skills
        .iter()
        .map(|skill| {
            (
                skill.name.as_str(),
                skill.description.as_str(),
                skill.source,
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            ("review", "alpha workflow", SkillSource::WorkspaceClaude),
            ("review", "beta workflow", SkillSource::WorkspaceClaude),
            ("review", "global workflow", SkillSource::GlobalClaude),
        ]
    );
    assert_eq!(
        discovery.skills[0].path,
        workspace_root.join(".claude/skills/alpha")
    );
    assert_eq!(
        discovery.skills[1].path,
        workspace_root.join(".claude/skills/beta")
    );
    assert_eq!(
        discovery.skills[2].path,
        home_root.join(".claude/skills/global")
    );
    assert!(discovery.diagnostics.is_empty());
}

#[test]
fn load_visible_skills_stops_ancestor_walking_before_home_and_keeps_home_agents_global() {
    let fixture = Fixture::new();
    fixture.write(
        "home/skills/home-shared/SKILL.md",
        "---\nname: home-shared\ndescription: should not load\n---\n\nbody\n",
    );
    fixture.write(
        "home/.agents/skills/review/SKILL.md",
        "---\nname: review\ndescription: global agents\n---\n\nbody\n",
    );
    fixture.mkdir("home/workspace/app");

    let discovery = fixture
        .home_context("home/workspace/app")
        .load_visible_skills(&TEST_ROOT_POLICY);
    assert_eq!(discovery.skills.len(), 1);
    assert_eq!(discovery.skills[0].name, "review");
    assert_eq!(discovery.skills[0].source, SkillSource::GlobalAgents);
}

#[test]
fn load_visible_skills_discovers_workspace_and_global_codex_roots() {
    let fixture = Fixture::new();
    fixture.write(
        "home/workspace/app/.codex/skills/local-codex/SKILL.md",
        "---\nname: local-codex\ndescription: workspace codex\n---\n\nbody\n",
    );
    fixture.write(
        "home/.codex/skills/global-codex/SKILL.md",
        "---\nname: global-codex\ndescription: global codex\n---\n\nbody\n",
    );

    let discovery = fixture
        .home_context("home/workspace/app")
        .load_visible_skills(&TEST_ROOT_POLICY);
    assert_eq!(discovery.skills.len(), 2);
    let source = |name| named(&discovery, name).unwrap().source;
    assert_eq!(source("local-codex"), SkillSource::WorkspaceCodex);
    assert_eq!(source("global-codex"), SkillSource::GlobalCodex);
}

#[test]
fn load_visible_skills_skips_missing_or_unreadable_skill_files_without_failing() {
    let fixture = Fixture::new();
    fixture.write(
        "root/good/SKILL.md",
        "---\nname: good\ndescription: loads\n---\n\nbody\n",
    );
    fixture.mkdir("root/missing");
    fixture.mkdir("root/unreadable/SKILL.md");

    let discovery = fixture.managed_discovery("root");
    assert_eq!(discovery.skills.len(), 1);
    assert_eq!(discovery.skills[0].name, "good");
    let diagnostic = only_diagnostic(&discovery);
    assert_eq!(diagnostic.scope, SkillDiagnosticScope::Candidate);
    assert_eq!(diagnostic.cause, SkillDiagnosticCause::Unreadable);
}

#[test]
fn load_visible_skills_ignores_nested_installer_transaction_payloads() {
    let fixture = Fixture::new();
    fixture.write(
        "root/review/SKILL.md",
        "---\nname: review\ndescription: installed\n---\nbody\n",
    );
    fixture.write(
        "root/.review.transaction-1/staged/SKILL.md",
        "---\nname: review\ndescription: staged\n---\nbody\n",
    );
    fixture.write(
        "root/.review.transaction-1/backup/SKILL.md",
        "---\nname: review\ndescription: backup\n---\nbody\n",
    );

    let discovery = fixture.managed_discovery("root");
    assert_eq!(discovery.skills.len(), 1);
    assert_eq!(discovery.skills[0].name, "review");
    assert_eq!(discovery.skills[0].description, "installed");
    assert!(discovery.diagnostics.is_empty());
}

#[test]
fn skill_discovery_rejects_symlinked_metadata_outside_its_selected_root() {
    let fixture = Fixture::new();
    fixture.write(
        "outside/SKILL.md",
        "---\nname: external-secret\ndescription: must not escape\n---\nbody\n",
    );
    fixture.symlink("../../outside/SKILL.md", "root/linked/SKILL.md");

    let discovery = fixture.managed_discovery("root");
    assert!(discovery.skills.is_empty());
    let diagnostic = only_diagnostic(&discovery);
    assert_eq!(diagnostic.scope, SkillDiagnosticScope::Candidate);
    assert_eq!(diagnostic.cause, SkillDiagnosticCause::Unreadable);
}

#[test]
fn skill_discovery_rejects_a_symlinked_selected_root() {
    let fixture = Fixture::new();
    fixture.write(
        "real-root/external/SKILL.md",
        "---\nname: external\ndescription: must not load\n---\nbody\n",
    );
    fixture.symlink("real-root", "linked-root");

    let discovery = fixture.managed_discovery("linked-root");
    assert!(discovery.skills.is_empty());
    assert_eq!(
        only_diagnostic(&discovery).scope,
        SkillDiagnosticScope::Root
    );
}

#[test]
fn skill_discovery_reports_a_broken_symlinked_selected_root() {
    let fixture = Fixture::new();
    fixture.symlink("missing-root", "broken-root");

    let discovery = fixture.managed_discovery("broken-root");
    assert!(discovery.skills.is_empty());
    let diagnostic = only_diagnostic(&discovery);
    assert_eq!(diagnostic.scope, SkillDiagnosticScope::Root);
    assert_eq!(diagnostic.cause, SkillDiagnosticCause::Unreadable);
}

#[test]
fn skill_discovery_rejects_symlinked_ancestors_inside_an_automatic_root() {
    let fixture = Fixture::new();
    fixture.write(
        "outside/skills/external/SKILL.md",
        "---\nname: external\ndescription: must not load\n---\nbody\n",
    );
    fixture.mkdir("home/workspace");
    fixture.symlink("../../outside", "home/workspace/.agents");

    let discovery = fixture
        .home_context("home/workspace")
        .load_visible_skills(&TEST_ROOT_POLICY);
    assert!(discovery.skills.is_empty());
    let diagnostic = only_diagnostic(&discovery);
    assert_eq!(diagnostic.scope, SkillDiagnosticScope::Root);
    assert_eq!(diagnostic.cause, SkillDiagnosticCause::Unreadable);
}

#[test]
fn skill_discovery_reports_a_symlinked_automatic_root_whose_target_lacks_the_trailing_component() {
    let fixture = Fixture::new();
    fixture.mkdir("outside");
    fixture.mkdir("home/workspace");
    fixture.symlink("../../outside", "home/workspace/.agents");

    let discovery = fixture
        .home_context("home/workspace")
        .load_visible_skills(&TEST_ROOT_POLICY);
    assert!(discovery.skills.is_empty());
    let diagnostic = only_diagnostic(&discovery);
    assert_eq!(diagnostic.scope, SkillDiagnosticScope::Root);
    assert_eq!(diagnostic.cause, SkillDiagnosticCause::Unreadable);
}

#[test]
fn load_visible_skills_discovers_a_skill_whose_body_exceeds_the_old_discovery_cap() {
    let fixture = Fixture::new();
    let mut content = b"---\nname: too-large\ndescription: remains discoverable\n---\n".to_vec();
    content.resize(1024 * 1024 + 1, b'a');
    fixture.write("root/too-large/SKILL.md", content);

    let discovery = fixture.managed_discovery("root");
    assert_eq!(discovery.skills.len(), 1);
    assert_eq!(discovery.skills[0].name, "too-large");
    assert_eq!(discovery.skills[0].description, "remains discoverable");
    assert!(discovery.diagnostics.is_empty());
}

#[test]
fn load_visible_skills_orders_valid_candidates_and_diagnoses_invalid_metadata() {
    let fixture = Fixture::new();
    fixture.write("root/zeta/SKILL.md", "zeta body without frontmatter\n");
    fixture.write(
        "root/beta/SKILL.md",
        "---\nname: duplicate\n---\nbeta body\n",
    );
    fixture.write(
        "root/alpha/SKILL.md",
        "---\nname: duplicate\n---\nalpha body\n",
    );
    fixture.write("root/bad/SKILL.md", "---\nname: \"\"\n---\ninvalid body\n");
    let root = fixture.real("root");

    let discovery = fixture.managed_discovery("root");
    assert_eq!(discovery.skills.len(), 3);
    assert_eq!(discovery.skills[0].path, root.join("alpha"));
    assert_eq!(discovery.skills[1].path, root.join("beta"));
    assert_eq!(discovery.skills[2].name, "zeta");
    assert_eq!(discovery.skills[2].description, "");
    let diagnostic = only_diagnostic(&discovery);
    assert_eq!(diagnostic.path, root.join("bad"));
    assert_eq!(diagnostic.source, SkillSource::GlobalOhFx);
    assert_eq!(diagnostic.scope, SkillDiagnosticScope::Candidate);
    assert_eq!(
        diagnostic.cause,
        SkillDiagnosticCause::InvalidMetadata(InvalidMetadataCause::MissingName)
    );
}

#[test]
fn load_visible_skills_diagnoses_a_hostile_no_frontmatter_directory_name() {
    let fixture = Fixture::new();
    fixture.write("root/hostile\nname/SKILL.md", "body without frontmatter\n");

    let discovery = fixture.managed_discovery("root");
    assert!(discovery.skills.is_empty());
    let diagnostic = only_diagnostic(&discovery);
    assert_eq!(diagnostic.path, fixture.real("root").join("hostile\nname"));
    assert_eq!(diagnostic.source, SkillSource::GlobalOhFx);
    assert_eq!(
        diagnostic.cause,
        SkillDiagnosticCause::InvalidMetadata(InvalidMetadataCause::ControlByte)
    );
}

#[test]
fn missing_roots_stay_silent_while_files_in_their_place_are_reported() {
    let fixture = Fixture::new();
    fixture.write("file-root", "not a directory\n");

    assert_eq!(
        fixture.managed_discovery("absent/root"),
        SkillDiscovery::default()
    );
    let discovery = fixture.managed_discovery("file-root");
    let diagnostic = only_diagnostic(&discovery);
    assert_eq!(diagnostic.path, fixture.path("file-root"));
    assert_eq!(diagnostic.scope, SkillDiagnosticScope::Root);
}

#[test]
fn workspace_roots_report_a_file_in_place_of_a_skill_directory() {
    let fixture = Fixture::new();
    fixture.write("home/workspace/skills", "not a directory\n");

    let discovery = fixture
        .home_context("home/workspace")
        .load_visible_skills(&TEST_ROOT_POLICY);
    let diagnostic = only_diagnostic(&discovery);
    assert_eq!(
        diagnostic.path,
        fixture.real("home/workspace").join("skills")
    );
    assert_eq!(diagnostic.source, SkillSource::WorkspaceShared);
    assert_eq!(diagnostic.scope, SkillDiagnosticScope::Root);
}
