use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ofx_config::{ContextLimitName, ContextLimits};
use ofx_workspace::PathError;
use tokio_util::sync::CancellationToken;

use super::loading::{
    command_line, failed, in_place_rewrite_cases, in_place_rewrite_fixture, loaded, loader, off,
    rewrite_in_place, workflow_fixture,
};
use crate::skill_contract::{Skill, SkillSource};
use crate::skill_invocation::resource::SkillResourceRead;
use crate::skill_invocation::{ExecuteResult, Selected, SkillError, SkillInventory, SkillLoader};
use crate::skill_runtime::{SkillDiscovery, SymlinkAuthorities};

fn unbounded_loader<'a>(
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
}

#[test]
fn skill_resource_defaults_preserve_whole_and_legacy_reads() {
    let (_fixture, discovery) = workflow_fixture("MAIN DOCUMENT\n");
    let authorities = SymlinkAuthorities::default();
    let loader = unbounded_loader(&discovery, &authorities);
    let path = discovery.skills[0].path.as_path();
    let whole = loader
        .load_whole_by_location(path, Some("SKILL.md"))
        .unwrap();
    let ExecuteResult::Loaded(output) = &whole else {
        panic!("expected complete content: {whole:?}");
    };
    assert!(output.complete);
    assert!(output.model_output.contains("MAIN DOCUMENT"));
    for resource in [None, Some("")] {
        assert_eq!(
            loader.load_whole_by_location(path, resource).unwrap(),
            whole
        );
        for offset in [0, 4] {
            assert_eq!(
                loader
                    .load_by_identity("workflow", Some(path), resource, offset)
                    .unwrap(),
                loader
                    .load_by_identity("workflow", Some(path), Some("SKILL.md"), offset)
                    .unwrap()
            );
        }
    }
    for resource in [" ", "../outside.txt", "/outside.txt"] {
        assert_eq!(
            loader.load_whole_by_location(path, Some(resource)),
            Err(SkillError::InvalidSkillResourcePath),
            "{resource}"
        );
    }
    assert_eq!(
        loader.load_whole_by_location(path, Some("missing.md")),
        Err(SkillError::Path(PathError::FileNotFound))
    );
}

#[test]
fn whole_skill_content_names_the_location_and_resource() {
    let (fixture, discovery) = workflow_fixture("MAIN DOCUMENT\n");
    fixture.write("skills/workflow/reference.md", "REFERENCE BODY\n");
    let authorities = SymlinkAuthorities::default();
    let loader = loader(&discovery, &authorities);
    let path = discovery.skills[0].path.as_path();
    assert_eq!(
        loaded(loader.load_whole_by_location(path, Some("reference.md"))),
        format!(
            "<skill_content name=\"workflow\" location=\"{}\" resource=\"reference.md\" complete=\"true\">\nREFERENCE BODY\n\n</skill_content>",
            path.display()
        )
    );
    let missing = failed(loader.load_whole_by_location(Path::new("/missing<skill>"), None));
    assert_eq!(
        missing.model_output,
        "Skill \"\" was not found at advertised location \"/missing&lt;skill&gt;\". Refresh available skills and retry with an advertised name and location."
    );
    assert_eq!(missing.notice, None);
}

#[test]
fn whole_skill_content_with_nul_bytes_is_omitted_with_a_notice() {
    let (_fixture, discovery) = workflow_fixture("before\0after\n");
    let authorities = SymlinkAuthorities::default();
    let loader = unbounded_loader(&discovery, &authorities);
    let result = loader
        .load_whole_by_location(&discovery.skills[0].path, None)
        .unwrap();
    let ExecuteResult::Loaded(output) = result else {
        panic!("expected sanitized content: {result:?}");
    };
    assert!(output.complete);
    assert!(
        output
            .model_output
            .starts_with("binary or non-utf8 tool output omitted (")
    );
    assert_eq!(
        output.notice.as_deref(),
        Some("[context] Skill content was sanitized before delivery.\n")
    );
}

#[test]
fn whole_skill_reads_respect_explicit_bounds_cancellation_and_recovery() {
    let (_fixture, discovery) = workflow_fixture(&format!(
        "{}COMPLETE TAIL\n",
        "required step\n".repeat(2000)
    ));
    let authorities = SymlinkAuthorities::default();
    let path = discovery.skills[0].path.as_path();
    let limited = |file, chunk| {
        let mut loader = unbounded_loader(&discovery, &authorities);
        loader.limits.file = file;
        loader.limits.chunk = chunk;
        loader
    };

    let file_blocked = failed(
        limited(
            command_line(128),
            ContextLimits::default().get(ContextLimitName::SkillChunkBytes),
        )
        .load_whole_by_location(path, None),
    );
    assert!(file_blocked.model_output.contains("skill_file_bytes"));
    assert!(!file_blocked.model_output.contains("required step"));

    let chunk_blocked =
        failed(limited(off(), command_line(128)).load_whole_by_location(path, None));
    assert!(chunk_blocked.model_output.contains("skill_chunk_bytes"));
    assert!(!chunk_blocked.model_output.contains("required step"));

    let result_blocked = failed(
        limited(off(), off())
            .with_max_tool_result_bytes(1024)
            .load_whole_by_location(path, None),
    );
    assert!(result_blocked.model_output.len() <= 1024);
    assert!(
        result_blocked
            .model_output
            .contains("max_tool_result_bytes")
    );
    assert!(!result_blocked.model_output.contains("required step"));

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        limited(off(), off())
            .with_cancellation(&cancellation)
            .load_whole_by_location(path, None),
        Err(SkillError::Cancelled)
    );
    let cancellation = CancellationToken::new();
    let cancellable = limited(off(), off()).with_cancellation(&cancellation);
    let Selected::Ready(selection) = cancellable.select("workflow", Some(path)).unwrap() else {
        panic!("expected the current skill");
    };
    cancellation.cancel();
    assert_eq!(
        cancellable.load_whole(selection, "SKILL.md"),
        Err(SkillError::Cancelled)
    );

    let cancellation = CancellationToken::new();
    let recovered = limited(off(), off())
        .with_cancellation(&cancellation)
        .load_whole_by_location(path, None)
        .unwrap();
    let ExecuteResult::Loaded(output) = recovered else {
        panic!("expected complete content: {recovered:?}");
    };
    assert!(output.complete);
    assert!(output.model_output.contains("COMPLETE TAIL"));
}

#[test]
fn whole_skill_reads_reject_identity_changes_after_candidate_validation() {
    let (fixture, discovery) = workflow_fixture("ORIGINAL BODY\n");
    fixture.write("skills/workflow/reference.md", "REFERENCE BODY\n");
    let authorities = SymlinkAuthorities::default();
    let loader = unbounded_loader(&discovery, &authorities);
    for resource in ["SKILL.md", "reference.md"] {
        fs::write(
            fixture.path("skills/workflow/SKILL.md"),
            "---\nname: workflow\ndescription: helper\n---\nORIGINAL BODY\n",
        )
        .unwrap();
        let Selected::Ready(selection) = loader.select("workflow", None).unwrap() else {
            panic!("expected the current skill");
        };
        fs::write(
            fixture.path("skills/workflow/SKILL.md"),
            "---\nname: replaced\ndescription: helper\n---\nREPLACED BODY\n",
        )
        .unwrap();
        assert_eq!(
            loader.load_whole(selection, resource),
            Err(SkillError::SkillResourceChanged),
            "{resource}"
        );
    }
}

#[test]
fn whole_skill_reads_reject_validated_candidates_rewritten_in_place() {
    for (rewrite, file_limit) in in_place_rewrite_cases() {
        for resource in ["SKILL.md", "reference.md"] {
            let (fixture, discovery) = in_place_rewrite_fixture();
            let authorities = SymlinkAuthorities::default();
            let mut loader = unbounded_loader(&discovery, &authorities);
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
                loader.load_whole(selection, resource),
                Err(SkillError::SkillResourceChanged),
                "{rewrite} {file_limit:?} {resource}"
            );
        }
    }
}

#[test]
fn whole_skill_content_fits_the_tool_result_budget_exactly() {
    let (_fixture, discovery) = workflow_fixture("MAIN DOCUMENT\n");
    let authorities = SymlinkAuthorities::default();
    let path = discovery.skills[0].path.as_path();
    let complete =
        loaded(unbounded_loader(&discovery, &authorities).load_whole_by_location(path, None));
    let exact = unbounded_loader(&discovery, &authorities)
        .with_max_tool_result_bytes(complete.len())
        .load_whole_by_location(path, None);
    assert_eq!(loaded(exact), complete);
    let short = failed(
        unbounded_loader(&discovery, &authorities)
            .with_max_tool_result_bytes(complete.len() - 1)
            .load_whole_by_location(path, None),
    );
    assert_eq!(
        short.model_output,
        format!(
            "Complete skill content exceeds max_tool_result_bytes ({} bytes). No complete instructions were loaded.",
            complete.len() - 1
        )
    );
}

#[test]
fn whole_skill_content_at_a_path_that_is_not_utf_8_is_omitted_with_a_notice() {
    let content = "---\nname: cafe\n---\nCAFE BODY\n";
    let skill = Skill {
        name: "cafe".to_owned(),
        description: String::new(),
        path: PathBuf::from(OsStr::from_bytes(b"/skills/caf\xe9/SKILL.md")),
        source: SkillSource::GlobalOhFx,
        read_authority: None,
    };
    let read = SkillResourceRead {
        text: content.to_owned(),
        observed_bytes: content.len(),
    };
    let raw_len = "<skill_content name=\"cafe\" location=\"".len()
        + skill.path.as_os_str().len()
        + format!("\" resource=\"SKILL.md\" complete=\"true\">\n{content}\n</skill_content>").len();
    let authorities = SymlinkAuthorities::default();
    let loader = SkillLoader::new(
        SkillInventory {
            skills: &[],
            diagnostics: &[],
        },
        &authorities,
        &ContextLimits::default(),
    );
    let result = loader.whole(&skill, "SKILL.md", &read, usize::MAX);
    let ExecuteResult::Loaded(output) = result else {
        panic!("expected sanitized content: {result:?}");
    };
    assert_eq!(
        output.model_output,
        format!("binary or non-utf8 tool output omitted ({raw_len} bytes)")
    );
    assert_eq!(
        output.notice.as_deref(),
        Some("[context] Skill content was sanitized before delivery.\n")
    );
}
