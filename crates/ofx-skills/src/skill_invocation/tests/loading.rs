use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::process::Command;

use ofx_config::{
    ContextLimit, ContextLimitName, ContextLimitSource, ContextLimitValue, ContextLimits,
};
use ofx_workspace::PathError;
use tokio_util::sync::CancellationToken;

use crate::skill_contract::{ExecuteOutput, Skill, SkillDiagnosticCause, SkillSource};
use crate::skill_invocation::failures::{skill_chunk_notice, skill_file_blocked_notice};
use crate::skill_invocation::resource::{
    SkillResourceRead, read_skill_file, read_skill_resource, verify_read_identity,
};
use crate::skill_invocation::{ExecuteResult, Selected, SkillError, SkillInventory, SkillLoader};
use crate::skill_runtime::{
    CandidateOpen, OpenedSkillCandidate, SkillDiscovery, SymlinkAuthorities,
    open_validated_skill_candidate,
};
use crate::test_fixture::Fixture;

pub(super) fn command_line(bytes: usize) -> ContextLimit {
    ContextLimit {
        value: ContextLimitValue::Bytes(bytes),
        source: ContextLimitSource::CommandLine,
    }
}

pub(super) fn off() -> ContextLimit {
    ContextLimit {
        value: ContextLimitValue::Off,
        source: ContextLimitSource::CommandLine,
    }
}

pub(super) fn loader<'a>(
    discovery: &'a SkillDiscovery,
    authorities: &'a SymlinkAuthorities,
) -> SkillLoader<'a> {
    SkillLoader::new(
        SkillInventory {
            skills: &discovery.skills,
            diagnostics: &discovery.diagnostics,
        },
        authorities,
        &ContextLimits::default(),
    )
    .with_max_tool_result_bytes(64 * 1024)
}

pub(super) fn loaded(result: Result<ExecuteResult, SkillError>) -> String {
    match result.unwrap() {
        ExecuteResult::Loaded(output) => output.model_output,
        ExecuteResult::Failure(output) => panic!("expected a loaded skill: {output:?}"),
    }
}

pub(super) fn failed(result: Result<ExecuteResult, SkillError>) -> ExecuteOutput {
    match result.unwrap() {
        ExecuteResult::Failure(output) => output,
        ExecuteResult::Loaded(output) => panic!("expected a failure: {output:?}"),
    }
}

pub(super) fn workflow_fixture(body: &str) -> (Fixture, SkillDiscovery) {
    let fixture = Fixture::new();
    fixture.write(
        "skills/workflow/SKILL.md",
        format!("---\nname: workflow\ndescription: workflow helper\n---\n\n{body}"),
    );
    let discovery = fixture.managed_discovery("skills");
    assert_eq!(discovery.skills.len(), 1, "{discovery:?}");
    (fixture, discovery)
}

#[test]
fn skill_invocation_loads_installed_skill_content() {
    let (_fixture, discovery) = workflow_fixture("use the workflow skill\n");
    let authorities = SymlinkAuthorities::default();
    assert_eq!(
        loaded(loader(&discovery, &authorities).load_by_identity("workflow", None, None, 0)),
        "<skill_content name=\"workflow\" resource=\"SKILL.md\" offset=\"0\" next_offset=\"76\">\n---\nname: workflow\ndescription: workflow helper\n---\n\nuse the workflow skill\n\n</skill_content>"
    );
}

#[test]
fn stale_skill_catalogs_reject_mutated_candidates_before_loading() {
    for mutation in ["deleted", "invalid", "renamed"] {
        let (fixture, discovery) = workflow_fixture("ORIGINAL BODY\n");
        let skill_file = fixture.path("skills/workflow/SKILL.md");
        match mutation {
            "deleted" => fs::remove_file(&skill_file).unwrap(),
            "invalid" => fs::write(
                &skill_file,
                "---\ndescription: missing name\n---\n\nINVALID BODY\n",
            )
            .unwrap(),
            _ => fs::write(
                &skill_file,
                "---\nname: renamed\ndescription: renamed helper\n---\n\nRENAMED BODY\n",
            )
            .unwrap(),
        }

        let authorities = SymlinkAuthorities::default();
        let output = failed(loader(&discovery, &authorities).load_by_identity(
            "workflow",
            Some(&discovery.skills[0].path),
            None,
            0,
        ));
        let expected = if mutation == "renamed" {
            "does not match the skill advertised at location"
        } else {
            "was not found at advertised location"
        };
        assert!(output.model_output.contains(expected), "{mutation}");
        if mutation == "invalid" {
            assert!(
                output
                    .diagnostic_notice
                    .as_deref()
                    .unwrap()
                    .contains("metadata is invalid (missing_name)")
            );
        }
        for body in ["ORIGINAL BODY", "INVALID BODY", "RENAMED BODY"] {
            assert!(!output.model_output.contains(body), "{mutation}");
        }
    }
}

#[test]
fn skill_invocation_reads_validated_candidate_resources_after_path_replacement() {
    for (resource, original, replacement) in [
        ("SKILL.md", "ORIGINAL SKILL BODY", "REPLACEMENT SKILL BODY"),
        (
            "assets/data.txt",
            "ORIGINAL ASSET BODY",
            "REPLACEMENT ASSET BODY",
        ),
    ] {
        let fixture = Fixture::new();
        for (root, body) in [("skills", "ORIGINAL"), ("replacement", "REPLACEMENT")] {
            fixture.write(
                &format!("{root}/workflow/SKILL.md"),
                format!("---\nname: workflow\n---\n\n{body} SKILL BODY\n"),
            );
            fixture.write(
                &format!("{root}/workflow/assets/data.txt"),
                format!("{body} ASSET BODY\n"),
            );
        }
        let discovery = fixture.managed_discovery("skills");
        let authorities = SymlinkAuthorities::default();
        let loader = loader(&discovery, &authorities);
        let Selected::Ready(selection) = loader
            .select("workflow", Some(&discovery.skills[0].path))
            .unwrap()
        else {
            panic!("expected the current skill");
        };
        fs::rename(
            fixture.path("skills/workflow"),
            fixture.path("skills/workflow-original"),
        )
        .unwrap();
        fs::rename(
            fixture.path("replacement/workflow"),
            fixture.path("skills/workflow"),
        )
        .unwrap();

        let output = loaded(loader.load_chunk(selection, resource, 0));
        assert!(output.contains(original), "{resource}");
        assert!(!output.contains(replacement), "{resource}");
    }
}

pub(super) const IN_PLACE_REWRITES: [&str; 3] = [
    "---\nname: renamed\ndescription: renamed helper\n---\n\nRENAMED BODY\n",
    "---\nname: workflow\ndescription: workflow helper\n---\n\nREWRITTEN BODY\n",
    "---\nname: rewrites\ndescription: workflow helper\n---\n\nRENAMED  SKILL BODY\n",
];

pub(super) fn in_place_rewrite_cases() -> impl Iterator<Item = (&'static str, Option<ContextLimit>)>
{
    IN_PLACE_REWRITES
        .into_iter()
        .flat_map(|rewrite| [(rewrite, None), (rewrite, Some(command_line(4)))])
}

pub(super) fn in_place_rewrite_fixture() -> (Fixture, SkillDiscovery) {
    let (fixture, discovery) = workflow_fixture("ORIGINAL SKILL BODY\n");
    fixture.write("skills/workflow/reference.md", "REFERENCE BODY\n");
    (fixture, discovery)
}

pub(super) fn rewrite_in_place(fixture: &Fixture, content: &str) {
    let path = fixture.path("skills/workflow/SKILL.md");
    let validated = fs::metadata(&path).unwrap();
    let mut file = OpenOptions::new().write(true).open(&path).unwrap();
    file.write_all(content.as_bytes()).unwrap();
    file.set_len(u64::try_from(content.len()).unwrap()).unwrap();
    file.set_modified(validated.modified().unwrap()).unwrap();
    let rewritten = fs::metadata(&path).unwrap();
    assert_eq!(rewritten.ino(), validated.ino());
    assert_eq!(rewritten.modified().unwrap(), validated.modified().unwrap());
}

#[test]
fn skill_invocation_rejects_validated_candidates_rewritten_in_place() {
    let original =
        "---\nname: workflow\ndescription: workflow helper\n---\n\nORIGINAL SKILL BODY\n";
    assert_eq!(IN_PLACE_REWRITES[2].len(), original.len());
    for (rewrite, file_limit) in in_place_rewrite_cases() {
        for resource in ["SKILL.md", "reference.md"] {
            let (fixture, discovery) = in_place_rewrite_fixture();
            let authorities = SymlinkAuthorities::default();
            let mut loader = loader(&discovery, &authorities);
            if let Some(limit) = file_limit {
                loader.limits.file = limit;
            }
            let Selected::Ready(selection) = loader
                .select("workflow", Some(&discovery.skills[0].path))
                .unwrap()
            else {
                panic!("expected the current skill");
            };
            rewrite_in_place(&fixture, rewrite);
            assert_eq!(
                loader.load_chunk(selection, resource, 0),
                Err(SkillError::SkillResourceChanged),
                "{rewrite} {file_limit:?} {resource}"
            );
        }
    }
}

#[test]
fn skill_reads_check_the_selected_name_in_delivered_or_bounded_frontmatter() {
    let (_fixture, discovery) = workflow_fixture("BODY\n");
    let workflow = &discovery.skills[0];
    let renamed = Skill {
        name: "renamed".to_owned(),
        ..workflow.clone()
    };
    let authorities = SymlinkAuthorities::default();
    let CandidateOpen::Current(candidate) = open_validated_skill_candidate(workflow, &authorities)
    else {
        panic!("expected the current skill");
    };
    let read = |text: &str, observed_bytes: usize| SkillResourceRead {
        text: text.to_owned(),
        observed_bytes,
    };
    let delivered = "---\nname: renamed\n---\n";
    let whole = read(delivered, delivered.len());
    assert_eq!(
        verify_read_identity(&candidate, workflow, &whole),
        Err(SkillError::SkillResourceChanged)
    );
    assert_eq!(verify_read_identity(&candidate, &renamed, &whole), Ok(()));

    let cut_before_frontmatter_ends = read("---\n", 64);
    assert_eq!(
        verify_read_identity(&candidate, workflow, &cut_before_frontmatter_ends),
        Ok(())
    );
    assert_eq!(
        verify_read_identity(&candidate, &renamed, &cut_before_frontmatter_ends),
        Err(SkillError::SkillResourceChanged)
    );
}

#[test]
fn skill_loads_stop_when_cancelled_before_or_after_selection() {
    let (_fixture, discovery) = workflow_fixture("body\n");
    let authorities = SymlinkAuthorities::default();
    let cancellation = CancellationToken::new();
    let loader = loader(&discovery, &authorities).with_cancellation(&cancellation);
    let Selected::Ready(selection) = loader.select("workflow", None).unwrap() else {
        panic!("expected the current skill");
    };
    cancellation.cancel();
    assert_eq!(
        loader.load_chunk(selection, "SKILL.md", 0),
        Err(SkillError::Cancelled)
    );
    assert_eq!(
        loader.load_by_identity("workflow", None, None, 0),
        Err(SkillError::Cancelled)
    );
}

#[test]
fn skill_file_validation_completes_utf_8_across_the_safety_ceiling() {
    let fixture = Fixture::new();
    fixture.write(
        "skills/valid/SKILL.md",
        "---\nname: valid\n---\none\n\u{20ac}\ntail\n",
    );
    fixture.write(
        "skills/invalid/SKILL.md",
        b"---\nname: invalid\n---\none\n\xe2x\n",
    );
    let discovery = fixture.managed_discovery("skills");
    let authorities = SymlinkAuthorities::default();
    let read = |index: usize, header: &str| {
        let CandidateOpen::Current(candidate) =
            open_validated_skill_candidate(&discovery.skills[index], &authorities)
        else {
            panic!("expected the current skill");
        };
        let ceiling = header.len() + 1;
        read_skill_file(
            candidate.skill_file(),
            candidate.freshness(),
            command_line(ceiling),
            ceiling,
            None,
        )
    };

    assert_eq!(
        read(0, "---\nname: invalid\n---\none\n").unwrap_err(),
        SkillError::BinarySkillResource
    );
    let valid = read(1, "---\nname: valid\n---\none\n").unwrap();
    assert_eq!(valid.text, "---\nname: valid\n---\none\n");
    assert_eq!(
        valid.observed_bytes,
        "---\nname: valid\n---\none\n\u{20ac}\ntail\n".len()
    );
}

#[test]
fn large_skills_stay_discoverable_and_explicit_overrides_can_continue_past_default_file_bounds() {
    let fixture = Fixture::new();
    let header = "---\nname: large\ndescription: large but discoverable\n---\n\n";
    let tail = "LARGE_SKILL_TAIL_SENTINEL\n";
    let mut body = header.to_owned();
    body.push_str(&"x".repeat(1024 * 1024 + 128 - header.len() - tail.len()));
    body.push_str(tail);
    fixture.write("skills/large/SKILL.md", body);
    let discovery = fixture.managed_discovery("skills");
    assert_eq!(discovery.skills.len(), 1);
    assert_eq!(discovery.skills[0].name, "large");
    let authorities = SymlinkAuthorities::default();

    let bounded = loader(&discovery, &authorities).load_by_identity("large", None, None, 0);
    let bounded = loaded(bounded);
    assert!(bounded.contains("name=\"skill_file_bytes\" action=\"blocked_remainder\""));
    assert!(!bounded.contains(tail));
    let mut unbounded = loader(&discovery, &authorities);
    unbounded.limits.file = off();
    unbounded.limits.chunk = off();
    unbounded.max_tool_result_bytes = None;
    assert!(loaded(unbounded.load_by_identity("large", None, None, 0)).contains(tail));
}

#[test]
fn skill_files_continue_on_line_safe_utf_8_boundaries_and_reject_binary_content() {
    let fixture = Fixture::new();
    fixture.write(
        "skills/workflow/SKILL.md",
        "---\nname: workflow\n---\none\n\u{4e8c}\nthree\n",
    );
    fixture.write(
        "skills/binary/SKILL.md",
        b"---\nname: binary\n---\n\xff\xfe",
    );
    let discovery = fixture.managed_discovery("skills");
    let authorities = SymlinkAuthorities::default();
    let mut loader = loader(&discovery, &authorities);
    loader.limits.chunk = command_line(27);
    loader.limits.file = command_line(100);

    let first = loaded(loader.load_by_identity("workflow", None, None, 0));
    assert_eq!(
        first,
        "<skill_content name=\"workflow\" resource=\"SKILL.md\" offset=\"0\" next_offset=\"27\">\n---\nname: workflow\n---\none\n\n<context_limit name=\"skill_chunk_bytes\" action=\"truncated\" observed_bytes=\"37\" effective_bytes=\"27\" source=\"command line\" next_offset=\"27\" override=\"--context-limit skill_chunk_bytes=BYTES|off\" />\n</skill_content>"
    );
    loader.limits.chunk = command_line(5);
    let second = loaded(loader.load_by_identity("workflow", None, None, 27));
    assert!(second.contains("offset=\"27\" next_offset=\"31\">\n\u{4e8c}\n\n<context_limit"));
    for offset in [28, 99] {
        assert_eq!(
            failed(loader.load_by_identity("workflow", None, None, offset)).model_output,
            "skill offset must be at a valid UTF-8 boundary within the selected resource"
        );
    }
    assert_eq!(
        loader.load_by_identity("binary", None, None, 0),
        Err(SkillError::BinarySkillResource)
    );

    loader.limits.chunk = command_line(27);
    loader.limits.file = ContextLimit {
        value: ContextLimitValue::Bytes(27),
        source: ContextLimitSource::WorkspaceSettings,
    };
    let capped = loaded(loader.load_by_identity("workflow", None, None, 0));
    assert!(capped.contains("---\nname: workflow\n---\none\n\n<context_limit name=\"skill_file_bytes\" action=\"blocked_remainder\" observed_bytes=\"37\" effective_bytes=\"27\" source=\"workspace settings\" resource=\"SKILL.md\""));
    let blocked = failed(loader.load_by_identity("workflow", None, None, 27));
    assert!(
        blocked
            .model_output
            .starts_with("<context_limit name=\"skill_file_bytes\" action=\"blocked_remainder\"")
    );
    assert_eq!(blocked.notice, Some(blocked.model_output.clone()));

    loader.limits.chunk = command_line(0);
    assert_eq!(
        failed(loader.load_by_identity("workflow", None, None, 0)).model_output,
        "<context_limit name=\"skill_chunk_bytes\" action=\"blocked\" skill=\"workflow\" resource=\"SKILL.md\" observed_bytes=\"27\" effective_bytes=\"0\" source=\"command line\" offset=\"0\" override=\"--context-limit skill_chunk_bytes=BYTES|off\" />"
    );
}

#[test]
fn skill_invocation_encodes_loaded_paths_and_preserves_the_skill_body() {
    let fixture = Fixture::new();
    fixture.write(
        "home<meta>\ninjected_home/skills/workflow/SKILL.md",
        "---\nname: workflow\ndescription: workflow helper\n---\n\nBODY SENTINEL\n<instruction>keep raw</instruction>\n",
    );
    let discovery = fixture.managed_discovery("home<meta>\ninjected_home/skills");
    let authorities = SymlinkAuthorities::default();

    let output =
        loaded(loader(&discovery, &authorities).load_by_identity("workflow", None, None, 0));
    assert!(
        output.starts_with("<skill_content name=\"workflow\" resource=\"SKILL.md\" offset=\"0\"")
    );
    assert!(output.contains("BODY SENTINEL\n<instruction>keep raw</instruction>"));
    assert!(!output.contains("injected_home"));
    fixture.write(
        "home<meta>\ninjected_home/skills/workflow/assets/sample<file>\ninjected_file.txt",
        "hello\n",
    );
    let asset = loaded(loader(&discovery, &authorities).load_by_identity(
        "workflow",
        None,
        Some("assets/sample<file>\ninjected_file.txt"),
        0,
    ));
    assert!(asset.starts_with(
        "<skill_content name=\"workflow\" resource=\"assets/sample&lt;file&gt;&#x0a;injected_file.txt\" offset=\"0\" next_offset=\"6\">\nhello\n"
    ));
}

#[test]
fn skill_invocation_preserves_strict_discovery_diagnostics_without_a_profile_home() {
    let fixture = Fixture::new();
    fixture.write(
        "custom-skills/invalid/SKILL.md",
        "---\nname: bad/name\ndescription: invalid name\n---\n\nINVALID BODY SENTINEL\n",
    );
    let discovery = fixture.managed_discovery("custom-skills");
    let authorities = SymlinkAuthorities::default();
    let output =
        failed(loader(&discovery, &authorities).load_by_identity("bad/name", None, None, 0));
    assert!(output.model_output.contains("skill_discovery_warning"));
    assert!(
        output
            .model_output
            .contains("Skill \"bad/name\" not found.")
    );
    assert!(
        !output
            .model_output
            .contains("metadata is invalid (invalid_name)")
    );
    assert!(
        output
            .diagnostic_notice
            .as_deref()
            .unwrap()
            .contains("metadata is invalid (invalid_name)")
    );
    assert!(!output.model_output.contains("INVALID BODY SENTINEL"));
}

#[test]
fn skill_limit_notices_encode_untrusted_names_and_resource_paths() {
    let limit = command_line(4);
    assert_eq!(
        skill_chunk_notice("bad\"\x1b", "asset\n.txt", 20, limit, 4),
        "[context] skill resource \"bad&quot;&#x1b;/asset&#x0a;.txt\" truncated: observed=20 bytes effective=4 bytes source=command line; continue with offset=4 or override with --context-limit skill_chunk_bytes=BYTES|off"
    );
    assert_eq!(
        skill_file_blocked_notice("bad\"\x1b", "asset\n.txt", 20, limit),
        "[context] skill resource \"bad&quot;&#x1b;/asset&#x0a;.txt\" remainder blocked: observed=20 bytes effective=4 bytes source=command line; override with --context-limit skill_file_bytes=BYTES|off"
    );
}

fn resource_fixture() -> (Fixture, SkillDiscovery) {
    let fixture = Fixture::new();
    fixture.write(
        "skills/workflow/SKILL.md",
        "---\nname: workflow\ndescription: helper\n---\n\nbody\n",
    );
    fixture.write("skills/workflow/assets/data.txt", "one\n\u{4e8c}\nthree\n");
    fixture.write("skills/workflow/reference.md", "REFERENCE BODY\n");
    let discovery = fixture.managed_discovery("skills");
    assert_eq!(discovery.skills.len(), 1, "{discovery:?}");
    (fixture, discovery)
}

fn open_candidate(skill: &Skill) -> OpenedSkillCandidate {
    match open_validated_skill_candidate(skill, &SymlinkAuthorities::default()) {
        CandidateOpen::Current(candidate) => candidate,
        other => panic!("expected the current skill: {other:?}"),
    }
}

#[test]
fn skill_invocation_loads_relative_resources_of_the_selected_skill() {
    let (_fixture, discovery) = resource_fixture();
    let authorities = SymlinkAuthorities::default();
    let loader = loader(&discovery, &authorities);
    assert_eq!(
        loaded(loader.load_by_identity(
            "workflow",
            Some(&discovery.skills[0].path),
            Some(" reference.md\n"),
            0
        )),
        "<skill_content name=\"workflow\" resource=\" reference.md&#x0a;\" offset=\"0\" next_offset=\"15\">\nREFERENCE BODY\n\n</skill_content>"
    );
    for resource in [None, Some(""), Some("SKILL.md"), Some(" SKILL.md\t")] {
        let output = loaded(loader.load_by_identity("workflow", None, resource, 0));
        assert!(output.contains("resource=\"") && output.contains("\n---\nname: workflow"));
    }
    assert_eq!(
        loader
            .load_by_identity("workflow", None, Some("./SKILL.md"), 0)
            .unwrap_err(),
        SkillError::InvalidSkillResourcePath
    );
}

#[test]
fn skill_references_reject_primary_identity_changes_after_candidate_validation() {
    let (fixture, discovery) = resource_fixture();
    let authorities = SymlinkAuthorities::default();
    let loader = loader(&discovery, &authorities);
    let Selected::Ready(selection) = loader.select("workflow", None).unwrap() else {
        panic!("expected the current skill");
    };
    fs::write(
        fixture.path("skills/workflow/SKILL.md"),
        "---\nname: renamed\n---\nCHANGED BODY\n",
    )
    .unwrap();
    assert_eq!(
        loader.load_chunk(selection, "reference.md", 0),
        Err(SkillError::SkillResourceChanged)
    );
}

#[test]
fn skill_resource_validation_completes_utf_8_across_the_safety_ceiling() {
    let fixture = Fixture::new();
    fixture.write(
        "skills/workflow/SKILL.md",
        "---\nname: workflow\n---\n\nworkflow\n",
    );
    fixture.write("skills/workflow/assets/valid.txt", "one\n\u{20ac}\ntail\n");
    fixture.write("skills/workflow/assets/invalid.txt", b"one\n\xe2x\n");
    let discovery = fixture.managed_discovery("skills");
    let candidate = open_candidate(&discovery.skills[0]);
    let limit = command_line(5);

    let read = read_skill_resource(&candidate, "assets/valid.txt", limit, 5, None).unwrap();
    assert_eq!(read.text, "one\n");
    assert_eq!(read.observed_bytes, "one\n\u{20ac}\ntail\n".len());
    assert_eq!(
        read_skill_resource(&candidate, "assets/invalid.txt", limit, 5, None).unwrap_err(),
        SkillError::BinarySkillResource
    );
}

#[test]
fn skill_resource_loading_rejects_symlink_components_and_candidate_rebinding() {
    let fixture = Fixture::new();
    fixture.write(
        "root/workflow/SKILL.md",
        "---\nname: workflow\n---\n\nworkflow\n",
    );
    fixture.write(
        "root/workflow/real-assets/data.txt",
        "INTERNAL_SYMLINK_SENTINEL",
    );
    fixture.write("root/workflow/real.txt", "FINAL_SYMLINK_SENTINEL");
    fixture.write("root/workflow/real-dir/nested.txt", "NESTED");
    fixture.write("root/workflow/fifo-dir/.keep", "");
    assert!(
        Command::new("mkfifo")
            .arg(fixture.path("root/workflow/fifo-dir/pipe"))
            .status()
            .unwrap()
            .success()
    );
    fixture.write("outside/SKILL.md", "OUTSIDE_SWAP_SENTINEL");
    fixture.write(
        "outside-root/skills/workflow/SKILL.md",
        "ANCESTOR_SYMLINK_SENTINEL",
    );
    fixture.symlink("real-assets", "root/workflow/assets");
    fixture.symlink("real.txt", "root/workflow/alias.txt");
    fixture.symlink("../outside-root", "ancestor-root/.agents");
    let discovery = fixture.managed_discovery("root");
    let candidate = open_candidate(&discovery.skills[0]);
    let limit = ContextLimits::default().get(ContextLimitName::SkillFileBytes);
    let read = |resource: &str| read_skill_resource(&candidate, resource, limit, usize::MAX, None);

    assert_eq!(
        read("assets/data.txt").unwrap_err(),
        SkillError::Path(PathError::NotDir)
    );
    assert_eq!(
        read("alias.txt").unwrap_err(),
        SkillError::Path(PathError::SymLinkLoop)
    );
    assert_eq!(
        read("real-dir").unwrap_err(),
        SkillError::Path(PathError::IsDir)
    );
    assert_eq!(
        read("fifo-dir/pipe").unwrap_err(),
        SkillError::InvalidSkillResource
    );
    assert_eq!(
        read("missing.md").unwrap_err(),
        SkillError::Path(PathError::FileNotFound)
    );
    for traversal in [
        " ",
        "../outside.txt",
        "/outside.txt",
        "real-dir/../real.txt",
    ] {
        assert_eq!(
            read(traversal).unwrap_err(),
            SkillError::InvalidSkillResourcePath,
            "{traversal}"
        );
    }
    for missing in ["missing/a/b", "missing/a/../b"] {
        assert_eq!(
            read(missing).unwrap_err(),
            SkillError::Path(PathError::FileNotFound),
            "{missing}"
        );
    }
    assert_eq!(read("real-dir\\nested.txt").unwrap().text, "NESTED");

    let ancestor = Skill {
        name: "workflow".to_owned(),
        description: String::new(),
        path: fixture
            .real("ancestor-root")
            .join(".agents/skills/workflow"),
        source: SkillSource::GlobalOhFx,
        read_authority: None,
    };
    assert!(matches!(
        open_validated_skill_candidate(&ancestor, &SymlinkAuthorities::default()),
        CandidateOpen::Skipped(SkillDiagnosticCause::Unreadable)
    ));

    fs::rename(
        fixture.path("root/workflow"),
        fixture.path("root/original-workflow"),
    )
    .unwrap();
    fixture.symlink("../outside", "root/workflow");
    assert!(matches!(
        open_validated_skill_candidate(&discovery.skills[0], &SymlinkAuthorities::default()),
        CandidateOpen::Skipped(SkillDiagnosticCause::Unreadable)
    ));
}

#[test]
fn skill_resources_continue_on_line_safe_utf_8_boundaries_and_reject_unsafe_inputs() {
    let (fixture, discovery) = resource_fixture();
    fixture.write("skills/workflow/assets/binary.bin", [0xff, 0xfe]);
    for (name, content) in [
        ("invalid-before-limit.bin", &b"abc\xffz"[..]),
        ("invalid-at-limit.bin", b"abcd\xff"),
        ("invalid-after-limit.bin", b"abcde\xff"),
    ] {
        fixture.write(&format!("skills/workflow/assets/{name}"), content);
    }
    fixture.write(
        "skills/workflow/assets/cross-chunk.txt",
        format!("{}\u{20ac}\n", "a".repeat(16 * 1024 - 1)),
    );
    let authorities = SymlinkAuthorities::default();
    let mut loader = loader(&discovery, &authorities);
    loader.limits.chunk = command_line(7);
    loader.limits.file = command_line(100);
    let load = |loader: &SkillLoader<'_>, resource: &str, offset| {
        loader.load_by_identity("workflow", None, Some(resource), offset)
    };

    let first = loaded(load(&loader, "assets/data.txt", 0));
    assert!(first.contains("resource=\"assets/data.txt\" offset=\"0\" next_offset=\"4\""));
    assert!(first.contains("one\n"));
    assert!(first.contains("name=\"skill_chunk_bytes\" action=\"truncated\""));
    let second = loaded(load(&loader, "assets/data.txt", 4));
    assert!(second.contains("offset=\"4\" next_offset=\"8\""));
    assert!(second.contains("\u{4e8c}\n"));
    assert_eq!(
        load(&loader, "../outside.txt", 0),
        Err(SkillError::InvalidSkillResourcePath)
    );
    assert_eq!(
        load(&loader, "assets/binary.bin", 0),
        Err(SkillError::BinarySkillResource)
    );

    loader.limits.file = ContextLimit {
        value: ContextLimitValue::Bytes(4),
        source: ContextLimitSource::WorkspaceSettings,
    };
    for resource in [
        "assets/invalid-before-limit.bin",
        "assets/invalid-at-limit.bin",
        "assets/invalid-after-limit.bin",
    ] {
        assert_eq!(
            load(&loader, resource, 0),
            Err(SkillError::BinarySkillResource),
            "{resource}"
        );
    }
    let capped = loaded(load(&loader, "assets/data.txt", 0));
    assert!(capped.contains("one\n"));
    assert!(capped.contains("name=\"skill_file_bytes\" action=\"blocked_remainder\""));
    assert!(capped.contains("effective_bytes=\"4\" source=\"workspace settings\""));
    let blocked = failed(load(&loader, "assets/data.txt", 4));
    assert!(
        blocked
            .model_output
            .contains("name=\"skill_file_bytes\" action=\"blocked_remainder\"")
    );
    assert!(!blocked.model_output.contains("<skill_content"));

    loader.limits.file = command_line(16 * 1024);
    loader.limits.chunk = off();
    let cross_chunk = loaded(load(&loader, "assets/cross-chunk.txt", 0));
    assert!(cross_chunk.contains("name=\"skill_file_bytes\" action=\"blocked_remainder\""));
}
