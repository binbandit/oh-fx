use std::fs;
use std::io::Read;
use std::os::unix::fs::symlink;
use std::process::Command;

use super::*;
use crate::skill_contract::InvalidMetadataCause;
use crate::skill_runtime::skill_file::LinkedSkillFile;
use crate::skill_runtime::{
    CandidateOpen, SkillResolution, open_validated_skill_candidate, resolve_skill,
};
use crate::test_fixture::Fixture;

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

trait WorkspaceFixture {
    fn home_context(&self, workspace: &str) -> SkillDiscoveryContext;
}

impl WorkspaceFixture for Fixture {
    fn home_context(&self, workspace: &str) -> SkillDiscoveryContext {
        self.mkdir("home/.oh-fx/skills");
        SkillDiscoveryContext {
            workspace_root: Some(self.real(workspace)),
            home: Some(self.real("home")),
            managed_root: self.real("home/.oh-fx/skills"),
            symlink_authorities: SymlinkAuthorities::default(),
        }
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
    assert_eq!(
        resolve_skill(&discovery.skills, "review", None),
        SkillResolution::AmbiguousName
    );
    assert_eq!(
        resolve_skill(
            &discovery.skills,
            "review",
            Some(&workspace_root.join(".claude/skills/beta"))
        ),
        SkillResolution::Found(&discovery.skills[1])
    );
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
fn a_workspace_outside_home_scans_only_its_own_roots_and_never_its_ancestors() {
    let fixture = Fixture::new();
    for (path, name) in [
        ("shared/skills/planted/SKILL.md", "planted"),
        ("shared/workspace/.agents/skills/parent/SKILL.md", "parent"),
        ("shared/workspace/app/skills/local/SKILL.md", "local"),
    ] {
        fixture.write(path, format!("---\nname: {name}\ndescription: d\n---\n"));
    }
    let mut context = fixture.home_context("shared/workspace/app");
    let names = |context: &SkillDiscoveryContext| {
        context
            .load_visible_skills(&TEST_ROOT_POLICY)
            .skills
            .into_iter()
            .map(|skill| skill.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&context), ["local"]);
    context.home = None;
    assert_eq!(names(&context), ["local"]);
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
fn load_visible_skills_orders_valid_candidates_diagnoses_invalid_metadata_and_resolves_exact_identity()
 {
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

    let beta = root.join("beta");
    assert_eq!(
        resolve_skill(&discovery.skills, "duplicate", None),
        SkillResolution::AmbiguousName
    );
    assert_eq!(
        resolve_skill(&discovery.skills, "duplicate", Some(&beta)),
        SkillResolution::Found(&discovery.skills[1])
    );
    assert_eq!(
        resolve_skill(&discovery.skills, "other", Some(&beta)),
        SkillResolution::NameLocationMismatch
    );
    assert_eq!(
        resolve_skill(
            &discovery.skills,
            "duplicate",
            Some(Path::new("/outside/root"))
        ),
        SkillResolution::NotFound
    );
    assert_eq!(
        resolve_skill(&discovery.skills, "zeta", None),
        SkillResolution::Found(&discovery.skills[2])
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

fn linked_discovery(fixture: &Fixture, authorities: SymlinkAuthorities) -> SkillDiscovery {
    let mut context = fixture.home_context("home/workspace");
    context.symlink_authorities = authorities;
    context.load_visible_skills(&TEST_ROOT_POLICY)
}

fn candidate_directory(fixture: &Fixture, sub_path: &str) -> (PathBuf, OwnedFd) {
    let path = fixture.real("home/workspace").join(sub_path);
    let directory = open_directory(&path).unwrap();
    (path, directory)
}

fn open_linked_metadata(fixture: &Fixture, sub_path: &str) -> PrimarySkillFile {
    let (path, directory) = candidate_directory(fixture, sub_path);
    let authority = fixture.real("home/workspace");
    let candidate = SkillCandidate {
        directory: &directory,
        path: &path,
        read_authority: Some(&authority),
    };
    open_primary_skill_file(&candidate, &SymlinkAuthorities::default())
}

fn read_to_string(mut file: &fs::File) -> String {
    let mut content = String::new();
    file.read_to_string(&mut content).unwrap();
    content
}

#[test]
fn load_visible_skills_discovers_and_reopens_contained_linked_metadata() {
    let fixture = Fixture::new();
    fixture.write(
        "home/workspace/skill-source/linked-leaf/SKILL.md",
        "---\nname: linked-leaf\ndescription: contained metadata link\n---\n\nLINKED_LEAF_BODY\n",
    );
    fixture.symlink(
        "../../../skill-source/linked-leaf/SKILL.md",
        "home/workspace/.codex/skills/linked-leaf/SKILL.md",
    );

    let discovery = linked_discovery(&fixture, SymlinkAuthorities::default());
    assert_eq!(discovery.skills.len(), 1);
    assert!(discovery.diagnostics.is_empty());
    assert_eq!(discovery.skills[0].name, "linked-leaf");
    assert_eq!(
        discovery.skills[0].path,
        fixture
            .real("home/workspace")
            .join(".codex/skills/linked-leaf")
    );
    let CandidateOpen::Current(candidate) =
        open_validated_skill_candidate(&discovery.skills[0], &SymlinkAuthorities::default())
    else {
        panic!("expected the current skill");
    };
    assert!(read_to_string(candidate.skill_file()).contains("LINKED_LEAF_BODY"));
}

#[test]
fn linked_metadata_and_resources_stay_on_the_opened_candidate_after_rebinding() {
    let fixture = Fixture::new();
    fixture.write(
        "home/workspace/.codex/skills/linked/metadata.md",
        "---\nname: linked\n---\noriginal metadata",
    );
    fixture.write(
        "home/workspace/.codex/skills/linked/asset.txt",
        "original resource",
    );
    fixture.symlink(
        "metadata.md",
        "home/workspace/.codex/skills/linked/SKILL.md",
    );
    let discovery = linked_discovery(&fixture, SymlinkAuthorities::default());
    let CandidateOpen::Current(candidate) =
        open_validated_skill_candidate(&discovery.skills[0], &SymlinkAuthorities::default())
    else {
        panic!("expected the current skill");
    };

    fs::rename(
        fixture.path("home/workspace/.codex/skills/linked"),
        fixture.path("home/workspace/.codex/skills/original-linked"),
    )
    .unwrap();
    fixture.write(
        "home/workspace/.codex/skills/linked/metadata.md",
        "replacement metadata",
    );
    fixture.write(
        "home/workspace/.codex/skills/linked/asset.txt",
        "replacement resource",
    );
    fixture.symlink(
        "metadata.md",
        "home/workspace/.codex/skills/linked/SKILL.md",
    );

    assert!(read_to_string(candidate.skill_file()).ends_with("original metadata"));
    let (resource, _) = candidate.open_resource("asset.txt").unwrap();
    assert_eq!(read_to_string(&resource), "original resource");
}

#[test]
fn linked_metadata_reauthorizes_a_target_changed_after_preflight() {
    let fixture = Fixture::new();
    fixture.write(
        "home/workspace/source/SKILL.md",
        "---\nname: linked-leaf\n---\ninside\n",
    );
    fixture.write(
        "home/outside/SKILL.md",
        "---\nname: linked-leaf\n---\noutside\n",
    );
    fixture.symlink(
        "../../../source/SKILL.md",
        "home/workspace/.codex/skills/linked-leaf/SKILL.md",
    );
    let (path, directory) = candidate_directory(&fixture, ".codex/skills/linked-leaf");
    let authority = fixture.real("home/workspace");
    let authorities = SymlinkAuthorities::default();
    let preflight = LinkedSkillFile::preflight(&path, &authority, &authorities).unwrap();

    fs::remove_file(path.join("SKILL.md")).unwrap();
    symlink("../../../../outside/SKILL.md", path.join("SKILL.md")).unwrap();

    assert!(preflight.open(&directory).is_none());
}

#[test]
fn linked_metadata_reauthorizes_the_path_of_the_opened_file() {
    let fixture = Fixture::new();
    fixture.write(
        "home/workspace/source/SKILL.md",
        "---\nname: linked-leaf\n---\ninside\n",
    );
    fixture.mkdir("home/outside");
    fixture.symlink(
        "../../../source/SKILL.md",
        "home/workspace/.codex/skills/linked-leaf/SKILL.md",
    );
    let (path, directory) = candidate_directory(&fixture, ".codex/skills/linked-leaf");
    let authority = fixture.real("home/workspace");
    let authorities = SymlinkAuthorities::default();
    let preflight = LinkedSkillFile::preflight(&path, &authority, &authorities).unwrap();

    fs::rename(
        fixture.path("home/workspace/source"),
        fixture.path("home/outside/source"),
    )
    .unwrap();
    fixture.symlink("../outside/source", "home/workspace/source");

    assert!(preflight.open(&directory).is_none());
}

#[test]
fn linked_metadata_outside_authority_is_rejected_before_descriptor_open() {
    let fixture = Fixture::new();
    fixture.write(
        "home/outside/SKILL.md",
        "---\nname: linked-leaf\n---\noutside\n",
    );
    fixture.symlink(
        "../../../../outside/SKILL.md",
        "home/workspace/.codex/skills/linked-leaf/SKILL.md",
    );

    assert!(matches!(
        open_linked_metadata(&fixture, ".codex/skills/linked-leaf"),
        PrimarySkillFile::Rejected
    ));
}

#[test]
fn linked_metadata_fifo_is_rejected_before_descriptor_open() {
    let fixture = Fixture::new();
    fixture.mkdir("home/workspace");
    assert!(
        Command::new("mkfifo")
            .arg(fixture.path("home/workspace/metadata.fifo"))
            .status()
            .unwrap()
            .success()
    );
    fixture.symlink(
        "../../../metadata.fifo",
        "home/workspace/.codex/skills/fifo/SKILL.md",
    );

    assert!(matches!(
        open_linked_metadata(&fixture, ".codex/skills/fifo"),
        PrimarySkillFile::Rejected
    ));
}

#[test]
fn load_visible_skills_discovers_and_reopens_a_contained_linked_workspace_candidate() {
    let fixture = Fixture::new();
    fixture.write(
        "home/workspace/skill-source/linked-skill/SKILL.md",
        "---\nname: linked-skill\ndescription: contained link\n---\n\nLINKED_SKILL_BODY\n",
    );
    fixture.symlink(
        "../../skill-source/linked-skill",
        "home/workspace/.codex/skills/linked-skill",
    );

    let discovery = linked_discovery(&fixture, SymlinkAuthorities::default());
    assert_eq!(discovery.skills.len(), 1);
    assert_eq!(discovery.skills[0].name, "linked-skill");
    assert_eq!(
        discovery.skills[0].path,
        fixture
            .real("home/workspace")
            .join(".codex/skills/linked-skill")
    );
    assert_eq!(discovery.skills[0].source, SkillSource::WorkspaceCodex);
    assert!(discovery.diagnostics.is_empty());
    let authorities = SymlinkAuthorities::default();
    assert!(matches!(
        open_validated_skill_candidate(&discovery.skills[0], &authorities),
        CandidateOpen::Current(_)
    ));

    fixture.write(
        "home/outside-skill/SKILL.md",
        "---\nname: linked-skill\ndescription: outside\n---\n\nOUTSIDE_BODY_MUST_NOT_LOAD\n",
    );
    fs::remove_file(fixture.path("home/workspace/.codex/skills/linked-skill")).unwrap();
    fixture.symlink(
        "../../../outside-skill",
        "home/workspace/.codex/skills/linked-skill",
    );
    assert!(matches!(
        open_validated_skill_candidate(&discovery.skills[0], &authorities),
        CandidateOpen::Skipped(SkillDiagnosticCause::Unreadable)
    ));
}

#[test]
fn load_visible_skills_diagnoses_an_unavailable_linked_workspace_candidate() {
    let fixture = Fixture::new();
    fixture.symlink(
        "../../skill-source/missing-skill",
        "home/workspace/.codex/skills/missing-skill",
    );

    let discovery = linked_discovery(&fixture, SymlinkAuthorities::default());
    assert!(discovery.skills.is_empty());
    let diagnostic = only_diagnostic(&discovery);
    assert_eq!(
        diagnostic.path,
        fixture
            .real("home/workspace")
            .join(".codex/skills/missing-skill")
    );
    assert_eq!(diagnostic.source, SkillSource::WorkspaceCodex);
    assert_eq!(diagnostic.scope, SkillDiagnosticScope::Candidate);
    assert_eq!(
        diagnostic.cause,
        SkillDiagnosticCause::LinkedCandidateUnavailable
    );
}

#[test]
fn load_visible_skills_discovers_a_contained_linked_workspace_root() {
    let fixture = Fixture::new();
    fixture.write(
        "home/workspace/skill-root/root-linked/SKILL.md",
        "---\nname: root-linked\ndescription: linked root\n---\n\nROOT_LINKED_BODY\n",
    );
    fixture.symlink("../skill-root", "home/workspace/.codex/skills");

    let discovery = linked_discovery(&fixture, SymlinkAuthorities::default());
    let skill = named(&discovery, "root-linked").unwrap();
    assert_eq!(skill.source, SkillSource::WorkspaceCodex);
    assert_eq!(
        skill.read_authority.as_deref(),
        Some(fixture.real("home/workspace").as_path())
    );
    assert!(discovery.diagnostics.is_empty());
}

#[test]
fn load_visible_skills_diagnoses_an_escaping_linked_workspace_candidate() {
    let fixture = Fixture::new();
    fixture.write(
        "home/outside-skill/SKILL.md",
        "---\nname: escaping-skill\ndescription: outside\n---\n\nOUTSIDE_BODY_MUST_NOT_LOAD\n",
    );
    fixture.symlink(
        "../../../outside-skill",
        "home/workspace/.codex/skills/escaping-skill",
    );

    let discovery = linked_discovery(&fixture, SymlinkAuthorities::default());
    assert!(discovery.skills.is_empty());
    let diagnostic = only_diagnostic(&discovery);
    assert_eq!(
        diagnostic.path,
        fixture
            .real("home/workspace")
            .join(".codex/skills/escaping-skill")
    );
    assert_eq!(diagnostic.scope, SkillDiagnosticScope::Candidate);
    assert_eq!(
        diagnostic.cause,
        SkillDiagnosticCause::LinkedCandidateUnavailable
    );
}

#[test]
fn load_visible_skills_discovers_linked_metadata_through_external_authority() {
    let fixture = Fixture::new();
    fixture.write(
        "external-store/linked-leaf/SKILL.md",
        "---\nname: linked-leaf\ndescription: external metadata link\n---\n\nEXTERNAL_LEAF_BODY\n",
    );
    fixture.symlink(
        "../../../../../external-store/linked-leaf/SKILL.md",
        "home/workspace/.codex/skills/linked-leaf/SKILL.md",
    );
    let external = fixture.real("external-store");

    let discovery = linked_discovery(
        &fixture,
        SymlinkAuthorities::new(&[], Some(external.as_os_str())),
    );
    assert_eq!(discovery.skills.len(), 1);
    assert!(discovery.diagnostics.is_empty());
    assert_eq!(discovery.skills[0].name, "linked-leaf");
}

#[test]
fn load_visible_skills_discovers_a_linked_candidate_resolved_via_external_symlink_authority() {
    let fixture = Fixture::new();
    fixture.write(
        "external-store/linked-skill/SKILL.md",
        "---\nname: linked-skill\ndescription: external link\n---\n\nEXTERNAL_BODY\n",
    );
    fixture.symlink(
        "../../../../external-store/linked-skill",
        "home/workspace/.codex/skills/linked-skill",
    );
    let external = fixture.real("external-store");

    let discovery = linked_discovery(
        &fixture,
        SymlinkAuthorities::new(&[], Some(external.as_os_str())),
    );
    assert_eq!(discovery.skills.len(), 1);
    assert_eq!(discovery.skills[0].name, "linked-skill");
    assert!(discovery.diagnostics.is_empty());
}

#[test]
fn load_visible_skills_discovers_a_linked_candidate_through_configured_symlink_authorities() {
    let fixture = Fixture::new();
    fixture.write(
        "external-store/configured-skill/SKILL.md",
        "---\nname: configured-skill\ndescription: configured external link\n---\n\nCONFIGURED_BODY\n",
    );
    fixture.symlink(
        "../../../../external-store/configured-skill",
        "home/workspace/.codex/skills/configured-skill",
    );
    let external = fixture.real("external-store");
    let dotdot = external.join("..").join("external-store");

    let rejected = linked_discovery(
        &fixture,
        SymlinkAuthorities::new(&[PathBuf::from("external-store"), dotdot], None),
    );
    assert!(rejected.skills.is_empty());
    assert_eq!(
        only_diagnostic(&rejected).cause,
        SkillDiagnosticCause::LinkedCandidateUnavailable
    );

    let accepted = linked_discovery(&fixture, SymlinkAuthorities::new(&[external], None));
    assert_eq!(accepted.skills.len(), 1);
    assert_eq!(accepted.skills[0].name, "configured-skill");
    assert!(accepted.diagnostics.is_empty());

    let cleared = linked_discovery(&fixture, SymlinkAuthorities::new(&[], None));
    assert!(cleared.skills.is_empty());
    assert_eq!(
        only_diagnostic(&cleared).cause,
        SkillDiagnosticCause::LinkedCandidateUnavailable
    );
}

#[test]
fn load_visible_skills_still_rejects_external_symlinks_without_an_authority() {
    let fixture = Fixture::new();
    fixture.write(
        "external-store/escaping-skill/SKILL.md",
        "---\nname: escaping-skill\ndescription: outside\n---\n\nOUTSIDE_BODY_MUST_NOT_LOAD\n",
    );
    fixture.symlink(
        "../../../../external-store/escaping-skill",
        "home/workspace/.codex/skills/escaping-skill",
    );

    let discovery = linked_discovery(&fixture, SymlinkAuthorities::default());
    assert!(discovery.skills.is_empty());
    let diagnostic = only_diagnostic(&discovery);
    assert_eq!(
        diagnostic.path,
        fixture
            .real("home/workspace")
            .join(".codex/skills/escaping-skill")
    );
    assert_eq!(diagnostic.scope, SkillDiagnosticScope::Candidate);
    assert_eq!(
        diagnostic.cause,
        SkillDiagnosticCause::LinkedCandidateUnavailable
    );
}

#[test]
fn managed_roots_never_follow_linked_candidates() {
    let fixture = Fixture::new();
    fixture.write(
        "elsewhere/linked/SKILL.md",
        "---\nname: linked\ndescription: must not load\n---\nbody\n",
    );
    fixture.symlink("../elsewhere/linked", "root/linked");

    let discovery = fixture.managed_discovery("root");
    assert_eq!(discovery, SkillDiscovery::default());
}

#[test]
fn a_verified_directory_chain_covers_only_whole_leading_components() {
    let fixture = Fixture::new();
    fixture.mkdir("a/bc");
    fixture.symlink("bc", "a/b");
    let mut chain = DirectoryChain::default();
    assert!(chain.lacks(&fixture.path("a/bc/missing")));
    assert!(!chain.lacks(&fixture.path("a/b/missing")));
    assert!(chain.lacks(&fixture.path("a/bc/other/skills")));
    assert!(!chain.lacks(&fixture.path("a/bc")));
    assert!(!chain.lacks(&fixture.path("a/b")));
    assert_eq!(chain.covered_length(b"/x/y"), 0);
    let parent = fixture.path("a");
    let parent = parent.as_os_str().as_bytes();
    assert_eq!(chain.covered_length(parent), parent.len());
    let partial = fixture.path("a/b");
    assert_eq!(
        chain.covered_length(partial.as_os_str().as_bytes()),
        parent.len()
    );
}
