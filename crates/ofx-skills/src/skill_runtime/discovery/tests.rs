use std::fs;
use std::os::unix::fs::symlink;

use tempfile::TempDir;

use super::*;
use crate::skill_contract::InvalidMetadataCause;

const TEST_MANAGED_ROOT_POLICY: RootPolicy = RootPolicy {
    managed_root_source: Some(SkillSource::GlobalOhFx),
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

    fn managed_discovery(&self, managed: &str) -> SkillDiscovery {
        SkillDiscoveryContext {
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
