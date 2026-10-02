use std::path::{Path, PathBuf};

use ofx_config::{ContextLimitName, ContextLimits};
use tokio_util::sync::CancellationToken;

use super::loading::{
    command_line, in_place_rewrite_cases, in_place_rewrite_fixture, off, rewrite_in_place,
    workflow_fixture,
};
use crate::skill_contract::{Skill, SkillSource};
use crate::skill_invocation::{
    ExplicitBinding, ExplicitPromptSection, NoticeTone, SkillError, SkillInventory, SkillLoader,
};
use crate::skill_runtime::{SkillDiscovery, SymlinkAuthorities};
use crate::test_fixture::Fixture;

fn section(
    discovery: &SkillDiscovery,
    prompt: &str,
    bindings: &[ExplicitBinding<'_>],
) -> ExplicitPromptSection {
    let authorities = SymlinkAuthorities::default();
    explicit_loader(discovery, &authorities)
        .build_explicit_prompt_section(prompt, bindings)
        .unwrap()
}

fn explicit_loader<'a>(
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
    .with_max_tool_result_bytes(1024)
}

fn load_body(section: &ExplicitPromptSection) -> &str {
    &section.load_notice.as_ref().unwrap().body
}

fn static_skill(name: &str, path: &str, source: SkillSource) -> Skill {
    Skill {
        name: name.to_owned(),
        description: String::new(),
        path: PathBuf::from(path),
        source,
        read_authority: None,
    }
}

#[test]
fn explicit_skill_requests_report_ambiguous_names_without_selecting_a_source() {
    let discovery = SkillDiscovery {
        skills: vec![
            static_skill("review", "/workspace/review", SkillSource::WorkspaceOhFx),
            static_skill("review", "/global/review", SkillSource::GlobalOhFx),
        ],
        diagnostics: Vec::new(),
    };
    let section = section(&discovery, "$review this patch", &[]);
    assert!(section.text.contains("ambiguous"));
    assert!(!section.text.contains("already loaded"));
    let notice = section.load_notice.as_ref().unwrap();
    assert_eq!(notice.tone, NoticeTone::Warning);
    assert!(
        notice
            .body
            .contains("Requested skills \u{b7} 0 loaded \u{b7} 1 failed")
    );
    assert!(notice.body.contains("Could not load review:"));
    assert!(notice.body.contains("ambiguous name"));
    assert!(notice.body.contains("ctrl+o"));
    for hidden in ["Loaded skill review", "/workspace/review", "/global/review"] {
        assert!(!notice.body.contains(hidden), "{hidden}");
    }
    let details = section.load_details.as_deref().unwrap();
    assert!(details.contains("/workspace/review") && details.contains("/global/review"));
    assert_eq!(section.notice, None);
}

#[test]
fn explicit_ambiguity_lists_every_location_whose_name_differs_only_in_case() {
    let discovery = SkillDiscovery {
        skills: vec![
            static_skill("Review", "/workspace/review", SkillSource::WorkspaceOhFx),
            static_skill("review", "/global/review", SkillSource::GlobalOhFx),
        ],
        diagnostics: Vec::new(),
    };
    let failure = "Skill \"Review\" is ambiguous. Retry with the name and one advertised location: \"/workspace/review\", \"/global/review\".";
    for prompt in [
        "$review this patch",
        "/review this patch",
        "use the review skill",
    ] {
        let section = section(&discovery, prompt, &[]);
        assert_eq!(section.text.lines().last(), Some(failure), "{prompt}");
        assert_eq!(
            section.load_details.as_deref(),
            Some(format!("Could not load Review: {failure}\n").as_str()),
            "{prompt}"
        );
        assert!(
            load_body(&section).ends_with("\u{2514} Could not load Review: ambiguous name"),
            "{prompt}"
        );
    }
}

#[test]
fn explicit_skills_include_complete_instructions_beyond_the_default_chunk() {
    let (_fixture, discovery) = workflow_fixture(&format!(
        "BEGIN INSTRUCTIONS\n{}COMPLETE TAIL\n",
        "required step\n".repeat(2000)
    ));
    let first = section(&discovery, "$workflow", &[]);
    assert!(first.text.contains("BEGIN INSTRUCTIONS"));
    assert!(first.text.contains("COMPLETE TAIL"));
    let notice = first.load_notice.as_ref().unwrap();
    assert_eq!(notice.tone, NoticeTone::Neutral);
    assert_eq!(
        notice.body,
        "1 requested skill loaded\n\u{2514} Loaded skill workflow"
    );
    assert_eq!(first.load_details, None);

    let binding = ExplicitBinding {
        name: "workflow",
        path: &discovery.skills[0].path,
    };
    let repeated = section(&discovery, "$workflow $workflow", &[binding, binding]);
    assert_eq!(repeated.text, first.text);
    assert_eq!(load_body(&repeated), load_body(&first));

    let authorities = SymlinkAuthorities::default();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        explicit_loader(&discovery, &authorities)
            .with_cancellation(&cancellation)
            .build_explicit_prompt_section("$workflow", &[]),
        Err(SkillError::Cancelled)
    );
}

#[test]
fn explicit_sections_reject_skills_rewritten_in_place_after_selection() {
    for (rewrite, file_limit) in in_place_rewrite_cases() {
        let (fixture, discovery) = in_place_rewrite_fixture();
        let authorities = SymlinkAuthorities::default();
        let rewrite_skill = || rewrite_in_place(&fixture, rewrite);
        let mut loader = explicit_loader(&discovery, &authorities);
        if let Some(limit) = file_limit {
            loader.limits.file = limit;
        }
        loader.after_selection = Some(&rewrite_skill);
        assert_eq!(
            loader.build_explicit_prompt_section("$workflow", &[]),
            Err(SkillError::SkillResourceChanged),
            "{rewrite} {file_limit:?}"
        );
    }
}

#[test]
fn explicit_load_summary_reports_a_missing_bound_skill_without_success() {
    let section = section(
        &SkillDiscovery::default(),
        "Use the selected workflow",
        &[ExplicitBinding {
            name: "missing-workflow",
            path: Path::new("/unavailable/workflow"),
        }],
    );
    let body = load_body(&section);
    assert!(body.contains("Requested skills \u{b7} 0 loaded \u{b7} 1 failed"));
    assert!(body.contains("Could not load missing-workflow"));
    assert!(!body.contains("/unavailable/workflow"));
    assert!(
        section
            .load_details
            .as_deref()
            .unwrap()
            .contains("not found at advertised location")
    );
    assert!(!section.text.contains("<skill_content"));
}

#[test]
fn explicit_invocation_preserves_configured_skill_content_when_discovery_is_incomplete() {
    let fixture = Fixture::new();
    let header = "---\nname: large\ndescription: large explicit skill\n---\n\n";
    let tail = "EXPLICIT_SKILL_TAIL_SENTINEL\n";
    let mut body = "x".repeat(80 * 1024);
    body.replace_range(..header.len(), header);
    body.replace_range(body.len() - tail.len().., tail);
    fixture.write("home/.oh-fx/skills/large/SKILL.md", &body);
    fixture.write(
        "home/.oh-fx/skills/malformed/SKILL.md",
        "---\ndescription: missing name\n---\nMALFORMED BODY",
    );
    let discovery = fixture.managed_discovery("home/.oh-fx/skills");
    assert_eq!(discovery.skills.len(), 1);
    assert_eq!(discovery.diagnostics.len(), 1);
    let authorities = SymlinkAuthorities::default();
    let mut loader = explicit_loader(&discovery, &authorities);
    loader.limits.file = off();
    loader.limits.chunk = off();

    let explicit = loader.build_explicit_prompt_section("$large", &[]).unwrap();
    assert!(explicit.text.contains("skill_discovery_warning"));
    assert!(explicit.text.contains(tail));
    assert!(explicit.text.contains("</skill_content>"));
    assert!(!explicit.text.contains("tool result truncated"));
    assert_eq!(explicit.notice, None);
    assert!(explicit.diagnostic_notice.is_some());

    loader.limits.chunk = command_line(64);
    let limited = loader.build_explicit_prompt_section("$large", &[]).unwrap();
    let limit_notice = limited.notice.unwrap();
    let diagnostic_notice = limited.diagnostic_notice.unwrap();
    assert!(limit_notice.contains("skill_chunk_bytes"));
    assert!(!limit_notice.contains("skill discovery warning:"));
    assert!(diagnostic_notice.contains("skill discovery warning:"));
    assert!(!diagnostic_notice.contains("skill_chunk_bytes"));
}

#[test]
fn explicit_invocation_reports_user_byte_ceilings_without_partial_success() {
    let (_fixture, discovery) =
        workflow_fixture("FIRST INSTRUCTIONS\nSECOND INSTRUCTIONS\nTAIL MUST WAIT\n");
    let authorities = SymlinkAuthorities::default();
    let mut loader = explicit_loader(&discovery, &authorities);
    loader.limits.chunk = command_line(64);

    let explicit = loader
        .build_explicit_prompt_section("$workflow apply these instructions", &[])
        .unwrap();
    assert!(!explicit.text.contains("already loaded"));
    assert!(explicit.text.contains(
        "Follow each skill's complete instructions and required resources before substantive work."
    ));
    assert!(explicit.text.contains(
        "If a skill cannot be followed, state the blocker instead of silently substituting another workflow."
    ));
    assert!(!explicit.text.contains("<skill_content"));
    assert!(
        explicit
            .text
            .contains("name=\"skill_chunk_bytes\" action=\"blocked\"")
    );
    assert!(!explicit.text.contains("TAIL MUST WAIT"));
    assert!(explicit.notice.is_some());
    let body = load_body(&explicit);
    assert!(body.contains("0 loaded \u{b7} 1 failed"));
    assert!(body.contains("Could not load workflow"));
    assert!(!body.contains("Loaded skill workflow"));
    assert!(
        explicit
            .load_details
            .as_deref()
            .unwrap()
            .contains("skill_chunk_bytes")
    );

    let fuzzy = loader
        .build_explicit_prompt_section("please improve this workflow", &[])
        .unwrap();
    assert_eq!(fuzzy, ExplicitPromptSection::default());

    loader.limits.chunk = command_line(0);
    let zero_chunk = loader
        .build_explicit_prompt_section("$workflow", &[])
        .unwrap();
    assert!(
        zero_chunk
            .text
            .contains("name=\"skill_chunk_bytes\" action=\"blocked\"")
    );
    assert!(zero_chunk.notice.is_some());

    loader.limits.chunk = ContextLimits::default().get(ContextLimitName::SkillChunkBytes);
    loader.limits.file = command_line(0);
    let zero_file = loader
        .build_explicit_prompt_section("$workflow", &[])
        .unwrap();
    assert!(
        zero_file
            .text
            .contains("name=\"skill_file_bytes\" action=\"blocked_remainder\"")
    );
    assert!(zero_file.notice.is_some());
}

#[test]
fn explicit_sections_fail_rather_than_pass_the_safety_ceiling() {
    let (_fixture, mut discovery) = workflow_fixture("body\n");
    discovery.skills.extend([
        static_skill("review", "/workspace/review", SkillSource::WorkspaceOhFx),
        static_skill("review", "/global/review", SkillSource::GlobalOhFx),
    ]);
    let authorities = SymlinkAuthorities::default();
    let unbounded = |prompt| {
        explicit_loader(&discovery, &authorities)
            .build_explicit_prompt_section(prompt, &[])
            .unwrap()
    };
    for prompt in ["$workflow", "$workflow $review", "$review"] {
        let complete = unbounded(prompt);
        let mut loader = explicit_loader(&discovery, &authorities);
        loader.ceiling = complete.text.len();
        assert_eq!(
            loader.build_explicit_prompt_section(prompt, &[]),
            Ok(complete.clone()),
            "{prompt}"
        );
        loader.ceiling = complete.text.len() - 1;
        assert_eq!(
            loader.build_explicit_prompt_section(prompt, &[]),
            Err(SkillError::SkillContextTooLarge),
            "{prompt}"
        );
    }
    let mut loader = explicit_loader(&discovery, &authorities);
    loader.ceiling = unbounded("$workflow").text.len();
    assert_eq!(
        loader.build_explicit_prompt_section("$workflow $review", &[]),
        Err(SkillError::SkillContextTooLarge)
    );
}
