use std::fs;

use ofx_config::{ContextLimit, ContextLimitSource, ContextLimitValue, ContextLimits};
use tokio_util::sync::CancellationToken;

use crate::skill_contract::ExecuteOutput;
use crate::skill_invocation::failures::{skill_chunk_notice, skill_file_blocked_notice};
use crate::skill_invocation::resource::read_skill_file;
use crate::skill_invocation::{ExecuteResult, Selected, SkillError, SkillInventory, SkillLoader};
use crate::skill_runtime::{
    CandidateOpen, SkillDiscovery, SymlinkAuthorities, open_validated_skill_candidate,
};
use crate::test_fixture::Fixture;

fn command_line(bytes: usize) -> ContextLimit {
    ContextLimit {
        value: ContextLimitValue::Bytes(bytes),
        source: ContextLimitSource::CommandLine,
    }
}

fn off() -> ContextLimit {
    ContextLimit {
        value: ContextLimitValue::Off,
        source: ContextLimitSource::CommandLine,
    }
}

fn loader<'a>(
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

fn loaded(result: Result<ExecuteResult, SkillError>) -> String {
    match result.unwrap() {
        ExecuteResult::Loaded(output) => output.model_output,
        ExecuteResult::Failure(output) => panic!("expected a loaded skill: {output:?}"),
    }
}

fn failed(result: Result<ExecuteResult, SkillError>) -> ExecuteOutput {
    match result.unwrap() {
        ExecuteResult::Failure(output) => output,
        ExecuteResult::Loaded(output) => panic!("expected a failure: {output:?}"),
    }
}

fn workflow_fixture(body: &str) -> (Fixture, SkillDiscovery) {
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
        loaded(loader(&discovery, &authorities).load_by_identity("workflow", None, 0)),
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
    let (fixture, discovery) = workflow_fixture("ORIGINAL SKILL BODY\n");
    fixture.write(
        "replacement/workflow/SKILL.md",
        "---\nname: workflow\n---\n\nREPLACEMENT SKILL BODY\n",
    );
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

    let output = loaded(loader.load_chunk(selection, 0));
    assert!(output.contains("ORIGINAL SKILL BODY"));
    assert!(!output.contains("REPLACEMENT SKILL BODY"));
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
    assert_eq!(loader.load_chunk(selection, 0), Err(SkillError::Cancelled));
    assert_eq!(
        loader.load_by_identity("workflow", None, 0),
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
        read_skill_file(candidate.skill_file(), command_line(ceiling), ceiling, None)
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

    let bounded = loader(&discovery, &authorities).load_by_identity("large", None, 0);
    let bounded = loaded(bounded);
    assert!(bounded.contains("name=\"skill_file_bytes\" action=\"blocked_remainder\""));
    assert!(!bounded.contains(tail));
    let mut unbounded = loader(&discovery, &authorities);
    unbounded.limits.file = off();
    unbounded.limits.chunk = off();
    unbounded.max_tool_result_bytes = None;
    assert!(loaded(unbounded.load_by_identity("large", None, 0)).contains(tail));
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

    let first = loaded(loader.load_by_identity("workflow", None, 0));
    assert_eq!(
        first,
        "<skill_content name=\"workflow\" resource=\"SKILL.md\" offset=\"0\" next_offset=\"27\">\n---\nname: workflow\n---\none\n\n<context_limit name=\"skill_chunk_bytes\" action=\"truncated\" observed_bytes=\"37\" effective_bytes=\"27\" source=\"command line\" next_offset=\"27\" override=\"--context-limit skill_chunk_bytes=BYTES|off\" />\n</skill_content>"
    );
    loader.limits.chunk = command_line(5);
    let second = loaded(loader.load_by_identity("workflow", None, 27));
    assert!(second.contains("offset=\"27\" next_offset=\"31\">\n\u{4e8c}\n\n<context_limit"));
    for offset in [28, 99] {
        assert_eq!(
            failed(loader.load_by_identity("workflow", None, offset)).model_output,
            "skill offset must be at a valid UTF-8 boundary within the selected resource"
        );
    }
    assert_eq!(
        loader.load_by_identity("binary", None, 0),
        Err(SkillError::BinarySkillResource)
    );

    loader.limits.chunk = command_line(27);
    loader.limits.file = ContextLimit {
        value: ContextLimitValue::Bytes(27),
        source: ContextLimitSource::WorkspaceSettings,
    };
    let capped = loaded(loader.load_by_identity("workflow", None, 0));
    assert!(capped.contains("---\nname: workflow\n---\none\n\n<context_limit name=\"skill_file_bytes\" action=\"blocked_remainder\" observed_bytes=\"37\" effective_bytes=\"27\" source=\"workspace settings\" resource=\"SKILL.md\""));
    let blocked = failed(loader.load_by_identity("workflow", None, 27));
    assert!(
        blocked
            .model_output
            .starts_with("<context_limit name=\"skill_file_bytes\" action=\"blocked_remainder\"")
    );
    assert_eq!(blocked.notice, Some(blocked.model_output.clone()));

    loader.limits.chunk = command_line(0);
    assert_eq!(
        failed(loader.load_by_identity("workflow", None, 0)).model_output,
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

    let output = loaded(loader(&discovery, &authorities).load_by_identity("workflow", None, 0));
    assert!(
        output.starts_with("<skill_content name=\"workflow\" resource=\"SKILL.md\" offset=\"0\"")
    );
    assert!(output.contains("BODY SENTINEL\n<instruction>keep raw</instruction>"));
    assert!(!output.contains("injected_home"));
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
    let output = failed(loader(&discovery, &authorities).load_by_identity("bad/name", None, 0));
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
