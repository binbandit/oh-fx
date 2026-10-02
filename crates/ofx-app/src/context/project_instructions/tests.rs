use std::ffi::OsString;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;

use std::process::Command;

use ofx_config::{ContextLimitSource, ContextLimitValue, EMERGENCY_CEILING_BYTES};
use tempfile::TempDir;

use super::omissions::append_omission;
use super::*;

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

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn write(&self, relative: &str, contents: &[u8]) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    fn mkdir(&self, relative: &str) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn link(&self, target: &str, relative: &str) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink(target, path).unwrap();
    }

    fn gather(&self, workspace: &str, limits: InstructionLimits) -> ProjectContext {
        let home = self.path("home");
        let config = self.path("home/.config/oh-fx");
        gather_project_context(
            &self.path(workspace),
            ProfileLocation {
                home: Some(home.as_os_str()),
                config_directory: Some(&config),
            },
            limits,
        )
    }
}

fn visible(context: &ProjectContext) -> &str {
    context.content.as_deref().unwrap_or_default()
}

fn find(haystack: &str, needle: &str) -> usize {
    haystack
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} missing from {haystack:?}"))
}

fn defaults() -> InstructionLimits {
    InstructionLimits::from_limits(&ContextLimits::default())
}

fn with_file_limit(bytes: usize, source: ContextLimitSource) -> InstructionLimits {
    InstructionLimits {
        file: ContextLimit {
            value: ContextLimitValue::Bytes(bytes),
            source,
        },
        ..defaults()
    }
}

fn with_total_limit(bytes: usize, source: ContextLimitSource) -> InstructionLimits {
    InstructionLimits {
        total: ContextLimit {
            value: ContextLimitValue::Bytes(bytes),
            source,
        },
        ..defaults()
    }
}

fn limit(bytes: usize) -> ContextLimit {
    ContextLimit {
        value: ContextLimitValue::Bytes(bytes),
        source: ContextLimitSource::CommandLine,
    }
}

#[test]
fn ordering_helper_sorts_stably_in_place() {
    let mut pairs = vec![(3, 'a'), (1, 'b'), (3, 'c'), (2, 'd'), (1, 'e')];
    sort_with(&mut pairs, |left, right| left.0.cmp(&right.0));
    assert_eq!(pairs, [(1, 'b'), (1, 'e'), (2, 'd'), (3, 'a'), (3, 'c')]);
    let mut empty: Vec<u8> = Vec::new();
    sort_with(&mut empty, u8::cmp);
    assert!(empty.is_empty());
}

#[test]
fn context_formatting_preserves_section_order_and_separators() {
    let mut out = String::new();
    append_section(
        &mut out,
        "project-instructions-guidance",
        "apply local rules",
    );
    append_section_from(
        &mut out,
        "global-rules",
        Path::new("/home/fx/.fx/AGENTS.md"),
        "global instructions",
    );
    append_section_from(
        &mut out,
        "project-rules",
        Path::new("/work/AGENTS.md"),
        "project instructions",
    );
    assert_eq!(
        out,
        "<project-instructions-guidance>\napply local rules\n</project-instructions-guidance>\n\n<global-rules from=\"/home/fx/.fx/AGENTS.md\">\nglobal instructions\n</global-rules>\n\n<project-rules from=\"/work/AGENTS.md\">\nproject instructions\n</project-rules>"
    );
}

#[test]
fn context_formatting_omits_missing_sections_without_extra_blank_lines() {
    let mut guidance_only = String::new();
    append_section(
        &mut guidance_only,
        "project-instructions-guidance",
        "apply local rules",
    );
    assert_eq!(
        guidance_only,
        "<project-instructions-guidance>\napply local rules\n</project-instructions-guidance>"
    );
    let mut empty = String::new();
    append_section(&mut empty, "project-instructions-guidance", "");
    append_section_from(
        &mut empty,
        "global-rules",
        Path::new("/home/fx/.fx/AGENTS.md"),
        "",
    );
    append_section_from(
        &mut empty,
        "project-rules",
        Path::new("/work/AGENTS.md"),
        "",
    );
    assert_eq!(empty, "");
}

#[test]
fn rules_section_source_path_is_escaped_as_an_attribute() {
    let mut out = String::new();
    append_section_from(
        &mut out,
        "project-rules",
        Path::new("/tmp/a&b/'safe'/\"quoted\"/<rules>\ninjected_field: yes/AGENTS.md"),
        "project instructions",
    );
    assert_eq!(
        out,
        "<project-rules from=\"/tmp/a&amp;b/'safe'/&quot;quoted&quot;/&lt;rules&gt;&#x0a;injected_field: yes/AGENTS.md\">\nproject instructions\n</project-rules>"
    );
}

#[test]
fn scoped_provenance_and_omission_attributes_are_encoded() {
    let mut out = String::new();
    append_scoped_section_from(
        &mut out,
        Path::new("/tmp/a&b\"<source>\nAGENTS.md"),
        Path::new("/tmp/a&b\"<scope>\nchild"),
        "nested instructions",
    );
    append_omission(
        &mut out,
        Path::new("/tmp/a&b\"<omission>\ninjected"),
        OmissionReason::Unreadable,
    );
    assert!(out.contains("&amp;b&quot;&lt;source&gt;&#x0a;AGENTS.md"));
    assert!(out.contains("&amp;b&quot;&lt;scope&gt;&#x0a;child"));
    assert!(out.contains("&amp;b&quot;&lt;omission&gt;&#x0a;injected"));
    assert!(!out.contains("\ninjected"));
    assert!(out.ends_with(
        "<project-rules-omitted from=\"/tmp/a&amp;b&quot;&lt;omission&gt;&#x0a;injected\" reason=\"unreadable rule file\" />"
    ));
}

#[test]
fn project_instruction_file_cap_keeps_a_line_safe_prefix_and_reports_source_facts() {
    let fixture = Fixture::new();
    fixture.write("home/.config/oh-fx/AGENTS.md", b"GLOBAL-ONE\nGLOBAL-TWO\n");
    fixture.write("home/work/AGENTS.md", b"PROJECT-ONE\nPROJECT-TWO\n");
    let context = fixture.gather(
        "home/work",
        with_file_limit(12, ContextLimitSource::WorkspaceSettings),
    );
    let text = visible(&context);
    assert!(text.contains("GLOBAL-ONE"));
    assert!(!text.contains("GLOBAL-TWO"));
    assert!(text.contains("PROJECT-ONE"));
    assert!(text.contains("project_instruction_file_bytes"));
    assert_eq!(context.notices.len(), 2);
    assert!(context.notices[0].contains("effective=12 bytes"));
    assert!(context.notices[0].contains("source=workspace settings"));
}

#[test]
fn file_and_total_limit_markers_match_upstream_bytes() {
    let fixture = Fixture::new();
    let global = fixture.write("home/.config/oh-fx/AGENTS.md", b"GLOBAL-ONE\nGLOBAL-TWO\n");
    let project = fixture.write("home/work/AGENTS.md", b"LINE-ONE\nLINE-TWO\nLINE-THREE\n");
    let context = fixture.gather(
        "home/work",
        with_file_limit(12, ContextLimitSource::CommandLine),
    );
    let (global, project) = (global.display(), project.display());
    assert_eq!(
        visible(&context),
        format!(
            "<project-instructions-guidance>\n{GUIDANCE}\n</project-instructions-guidance>\n\n<global-rules from=\"{global}\">\nGLOBAL-ONE\n</global-rules>\n\n<context_limit name=\"project_instruction_file_bytes\" action=\"truncated\" source_file=\"{global}\" observed_bytes=\"22\" effective_bytes=\"12\" source=\"command line\" override=\"--context-limit project_instruction_file_bytes=BYTES|off\" />\n\n<project-rules from=\"{project}\">\nLINE-ONE\n</project-rules>\n\n<context_limit name=\"project_instruction_file_bytes\" action=\"truncated\" source_file=\"{project}\" observed_bytes=\"29\" effective_bytes=\"12\" source=\"command line\" override=\"--context-limit project_instruction_file_bytes=BYTES|off\" />"
        )
    );
    assert_eq!(
        context.notices,
        [
            format!(
                "[context] project instruction file \"{global}\" truncated: observed=22 bytes effective=12 bytes source=command line; override with --context-limit project_instruction_file_bytes=BYTES|off"
            ),
            format!(
                "[context] project instruction file \"{project}\" truncated: observed=29 bytes effective=12 bytes source=command line; override with --context-limit project_instruction_file_bytes=BYTES|off"
            ),
        ]
    );

    let context = fixture.gather(
        "home/work",
        with_total_limit(250, ContextLimitSource::GlobalSettings),
    );
    let full = fixture.gather("home/work", defaults());
    let observed = visible(&full).len();
    assert_eq!(
        visible(&context),
        format!(
            "<project-instructions-guidance>\n{GUIDANCE}\n</project-instructions-guidance>\n\n<context_limit name=\"project_instructions_total_bytes\" action=\"omitted\" omitted_count=\"2\" observed_bytes=\"{observed}\" effective_bytes=\"250\" source=\"global settings\" override=\"--context-limit project_instructions_total_bytes=BYTES|off\" />"
        )
    );
    assert_eq!(
        context.notices,
        [format!(
            "[context] project instructions omitted 2 source(s) ({global}, {project}): observed={observed} bytes effective=250 bytes source=global settings; override with --context-limit project_instructions_total_bytes=BYTES|off"
        )]
    );
}

#[test]
fn project_instruction_limit_notices_encode_untrusted_source_paths() {
    let prior = DeliveryState::default();
    let mut selection = Selection::new(
        Path::new("/work"),
        with_file_limit(4, ContextLimitSource::CommandLine),
        &prior,
    );
    let mut out = String::new();
    selection.append_file_limit_marker(&mut out, Path::new("bad\"\x1b\nAGENTS.md"), 20);
    assert_eq!(selection.notices.len(), 1);
    assert!(selection.notices[0].contains("bad&quot;&#x1b;&#x0a;AGENTS.md"));
    assert!(!selection.notices[0].contains('\x1b'));
}

#[test]
fn combined_project_instruction_cap_keeps_global_and_closest_whole_sections_in_render_order() {
    let full = format!(
        "GUIDE\n\nGLOBAL_BODY\n{}\n\nMIDDLE_BODY\n{}\n\nCLOSE_BODY\n{}\n\n<project-rules-omitted from=\"/unsafe/AGENTS.md\" reason=\"unsafe target\" />",
        "g".repeat(160),
        "m".repeat(400),
        "c".repeat(160)
    );
    let global_start = find(&full, "\n\nGLOBAL_BODY");
    let middle_start = find(&full, "\n\nMIDDLE_BODY");
    let close_start = find(&full, "\n\nCLOSE_BODY");
    let consequence_start = find(&full, "\n\n<project-rules-omitted");
    let rules = [
        RenderedRule {
            source: PathBuf::from("/global/AGENTS.md"),
            start: global_start,
            end: middle_start,
            is_global: true,
        },
        RenderedRule {
            source: PathBuf::from("/middle\"\x1b\n/AGENTS.md"),
            start: middle_start,
            end: close_start,
            is_global: false,
        },
        RenderedRule {
            source: PathBuf::from("/close/AGENTS.md"),
            start: close_start,
            end: consequence_start,
            is_global: false,
        },
    ];
    let mut notices = Vec::new();
    let capped = cap_total(&full, &rules, limit(600), &mut notices);
    let global_index = find(&capped, "GLOBAL_BODY");
    let close_index = find(&capped, "CLOSE_BODY");
    let preserved_omission_index = find(&capped, "<project-rules-omitted");
    assert!(global_index < close_index);
    assert!(
        capped[..preserved_omission_index]
            .trim_end_matches(['\r', '\n'])
            .len()
            <= 600
    );
    assert!(!capped.contains("MIDDLE_BODY"));
    assert!(capped.contains("/unsafe/AGENTS.md"));
    assert!(capped.contains("omitted_count=\"1\""));
    assert!(!capped.contains("/middle"));
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("/middle&quot;&#x1b;&#x0a;/AGENTS.md"));
    assert!(!notices[0].contains('\x1b'));

    let mut tiny_notices = Vec::new();
    let tiny = cap_total(&full, &rules, limit(0), &mut tiny_notices);
    assert!(tiny.contains("<project-rules-omitted from=\"/unsafe/AGENTS.md\""));
    assert!(tiny.contains("<context_limit name=\"project_instructions_total_bytes\""));
    assert!(tiny.contains("omitted_count=\"3\""));
    assert_eq!(tiny_notices.len(), 1);
}

#[test]
fn rule_loader_classifies_bodies_blanks_oversized_non_regular_and_symlink_files() {
    let fixture = Fixture::new();
    let default_limit = defaults().file.effective_bytes();
    let rules = fixture.write("rules.md", b" \t\nproject instructions\r\n ");
    assert_eq!(
        load_rule(&rules, default_limit),
        RuleLoad::Body(rule_file::RuleBody {
            text: "project instructions".to_owned(),
            observed_bytes: 26,
        })
    );
    let blank = fixture.write("blank.md", b" \t\r\n ");
    assert_eq!(load_rule(&blank, default_limit), RuleLoad::Blank);
    let oversized = fixture.write("oversized.md", &vec![b'x'; default_limit + 1]);
    match load_rule(&oversized, default_limit) {
        RuleLoad::Body(body) => {
            assert_eq!(body.observed_bytes, default_limit + 1);
            assert!(body.text.len() <= default_limit);
        }
        other => panic!("{other:?}"),
    }
    let late = fixture.write("late-content.md", b"        LATE");
    assert_eq!(
        load_rule(&late, 4),
        RuleLoad::Body(rule_file::RuleBody {
            text: String::new(),
            observed_bytes: 12,
        })
    );
    let invalid_after_cap = fixture.write("invalid-after-cap.md", b"valid\n\xff");
    assert_eq!(
        load_rule(&invalid_after_cap, 4),
        RuleLoad::Omitted(OmissionReason::Unreadable)
    );
    let split_character = fixture.write("split.md", "é".repeat(20_000).as_bytes());
    assert!(matches!(
        load_rule(&split_character, default_limit),
        RuleLoad::Body(_)
    ));
    let directory = fixture.mkdir("directory.md");
    assert_eq!(
        load_rule(&directory, default_limit),
        RuleLoad::Omitted(OmissionReason::NonRegular)
    );
    fixture.write("secret.md", b"contained linked instructions");
    fixture.link("secret.md", "linked.md");
    assert_eq!(
        load_rule(&fixture.path("linked.md"), default_limit),
        RuleLoad::Body(rule_file::RuleBody {
            text: "contained linked instructions".to_owned(),
            observed_bytes: 29,
        })
    );
    assert_eq!(
        load_rule(&fixture.path("missing.md"), default_limit),
        RuleLoad::Missing
    );
    assert_eq!(
        load_rule(&fixture.path("rules.md/AGENTS.md"), default_limit),
        RuleLoad::Missing
    );
}

#[test]
fn rule_loader_never_reads_through_escaping_or_intermediate_symlinks() {
    let fixture = Fixture::new();
    let limit = 1024;
    fixture.write("outside/secret.txt", b"DO_NOT_EXPOSE");
    fixture.write("work/CLAUDE.md", b"CONTAINED");
    fixture.link("../outside/secret.txt", "work/escape/AGENTS.md");
    fixture.link(
        fixture.path("outside/secret.txt").to_str().unwrap(),
        "work/absolute/AGENTS.md",
    );
    fixture.link("../CLAUDE.md", "work/parent/AGENTS.md");
    fixture.link("missing.md", "work/dangling/AGENTS.md");
    fixture.link("../outside", "work/through");
    fixture.write("outside/AGENTS.md", b"DO_NOT_EXPOSE");
    for relative in [
        "work/escape/AGENTS.md",
        "work/absolute/AGENTS.md",
        "work/parent/AGENTS.md",
        "work/dangling/AGENTS.md",
    ] {
        assert_eq!(
            load_rule(&fixture.path(relative), limit),
            RuleLoad::Omitted(OmissionReason::Symlink),
            "{relative}"
        );
    }
    assert_eq!(
        load_rule(&fixture.path("work/through/AGENTS.md"), limit),
        RuleLoad::Missing
    );
    assert!(
        Command::new("mkfifo")
            .arg(fixture.path("work/fifo.md"))
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        load_rule(&fixture.path("work/fifo.md"), limit),
        RuleLoad::Omitted(OmissionReason::NonRegular)
    );
    fixture.link("fifo.md", "work/fifo-link.md");
    assert_eq!(
        load_rule(&fixture.path("work/fifo-link.md"), limit),
        RuleLoad::Omitted(OmissionReason::Symlink)
    );
}

#[test]
fn a_contained_symlink_is_refused_when_its_directory_path_is_not_canonical() {
    let fixture = Fixture::new();
    fixture.write("real/CLAUDE.md", b"CONTAINED");
    fixture.link("CLAUDE.md", "real/AGENTS.md");
    fixture.link("real", "alias");
    assert_eq!(
        rule_file::open_contained_symlink(&fixture.path("alias"), &fixture.path("alias/AGENTS.md"))
            .map(|(_, size)| size),
        None
    );
    assert_eq!(
        rule_file::open_contained_symlink(&fixture.path("real"), &fixture.path("real/AGENTS.md"))
            .map(|(_, size)| size),
        Some(9)
    );
}

#[test]
fn initial_gather_loads_a_contained_agents_symlink_with_logical_provenance() {
    let fixture = Fixture::new();
    fixture.write("home/work/CLAUDE.md", b"LINKED_PROJECT_RULE");
    fixture.link("CLAUDE.md", "home/work/AGENTS.md");
    let context = fixture.gather("home/work", defaults());
    let logical = fixture.path("home/work/AGENTS.md");
    assert!(visible(&context).contains("LINKED_PROJECT_RULE"));
    assert!(visible(&context).contains(&format!("from=\"{}\"", logical.display())));
    assert!(!visible(&context).contains("symlinked rule file"));
}

#[test]
fn initial_gather_orders_global_ancestors_and_workspace_rules() {
    let fixture = Fixture::new();
    fixture.write("home/.config/oh-fx/AGENTS.md", b"RULE_GLOBAL");
    fixture.write("home/AGENTS.md", b"RULE_HOME");
    fixture.write("home/projects/AGENTS.md", b"RULE_PARENT");
    fixture.write("home/projects/group/AGENTS.md", b"RULE_GROUP");
    fixture.write("home/projects/group/work/AGENTS.md", b"RULE_WORKSPACE");
    fixture.write("home/projects/group/work/sub/AGENTS.md", b"RULE_NESTED");
    let context = fixture.gather("home/projects/group/work", defaults());
    let text = visible(&context);
    let order: Vec<usize> = [
        GUIDANCE,
        "RULE_GLOBAL",
        "RULE_PARENT",
        "RULE_GROUP",
        "RULE_WORKSPACE",
    ]
    .iter()
    .map(|needle| find(text, needle))
    .collect();
    assert!(order.is_sorted(), "{text}");
    assert!(!text.contains("RULE_HOME"));
    assert!(!text.contains("RULE_NESTED"));
    assert!(context.notices.is_empty());
    let parent = fixture.path("home/projects");
    assert!(text.contains(&format!(
        "<scoped-rules from=\"{}/AGENTS.md\" scope=\"{}\">\nRULE_PARENT\n</scoped-rules>",
        parent.display(),
        parent.display()
    )));
}

#[test]
fn initial_gather_renders_an_identical_global_and_workspace_source_once() {
    let fixture = Fixture::new();
    fixture.write(
        "home/.config/oh-fx/AGENTS.md",
        b"RULE_SHARED_GLOBAL_AND_WORKSPACE",
    );
    let context = fixture.gather("home/.config/oh-fx", defaults());
    let text = visible(&context);
    assert_eq!(text.matches("RULE_SHARED_GLOBAL_AND_WORKSPACE").count(), 1);
    assert!(text.contains("<global-rules"));
    assert!(!text.contains("<project-rules"));
    assert!(!text.contains("<scoped-rules"));
}

#[test]
fn a_missing_relative_configuration_directory_still_names_an_absolute_global_source() {
    let directory = Path::new("missing-relative-configuration/oh-fx");
    let source = global_rule_source(directory);
    assert_eq!(
        source,
        std::env::current_dir()
            .unwrap()
            .join(directory)
            .join(RULE_FILE_NAME)
    );
}

#[test]
fn initial_gather_keeps_the_nearest_ancestors_under_the_selection_cap() {
    let fixture = Fixture::new();
    let mut relative = String::from("home");
    for index in 0..36 {
        let _ = write!(relative, "/level-{index:02}");
        let body = match index {
            2 => " \n ".to_owned(),
            _ => format!("RULE_LEVEL_{index:02}"),
        };
        fixture.write(&format!("{relative}/AGENTS.md"), body.as_bytes());
    }
    let context = fixture.gather(&relative, defaults());
    let text = visible(&context);
    assert!(!text.contains("RULE_LEVEL_00"));
    assert!(!text.contains("RULE_LEVEL_01"));
    assert!(text.contains("RULE_LEVEL_03"));
    assert!(text.contains("RULE_LEVEL_34"));
    assert!(text.contains("RULE_LEVEL_35"));
    assert_eq!(text.matches("reason=\"selection cap\"").count(), 2);
    assert_eq!(text.matches("<scoped-rules").count(), 32);
    assert_eq!(context.notices.len(), 2);
}

#[test]
fn home_availability_failures_and_non_ancestor_diagnostics_stay_explicit() {
    let fixture = Fixture::new();
    let home = fixture.mkdir("home");
    let outside = fixture.mkdir("outside/work");
    let gather = |home: Option<&OsStr>, workspace: &Path| {
        gather_project_context(
            workspace,
            ProfileLocation {
                home,
                config_directory: Some(&home_config(&fixture)),
            },
            defaults(),
        )
    };
    let unavailable = gather(None, &outside);
    assert_eq!(
        visible(&unavailable),
        "<project-rules-omitted from=\"HOME\" reason=\"home unavailable\" />"
    );
    assert_eq!(
        unavailable.notices,
        [
            "[context] project instructions action=omitted reason=home unavailable source=\"HOME\"; repair=set HOME to an accessible directory"
        ]
    );
    let missing_home = fixture.path("missing-home");
    let unresolved = gather(Some(missing_home.as_os_str()), &outside);
    assert!(visible(&unresolved).contains(&format!(
        "<project-rules-omitted from=\"{}\" reason=\"home unavailable\" />",
        missing_home.display()
    )));
    let non_ancestor = gather(Some(home.as_os_str()), &outside);
    assert_eq!(
        visible(&non_ancestor),
        format!(
            "<project-rules-omitted from=\"{}\" reason=\"workspace is not below home\" />",
            outside.display()
        )
    );
    assert!(non_ancestor.notices.is_empty());
    let equal = gather(Some(home.as_os_str()), &home);
    assert_eq!(equal.content, None);
    assert!(equal.notices.is_empty());
    assert_eq!(equal.evaluated_endpoints, std::slice::from_ref(&home));
}

fn home_config(fixture: &Fixture) -> PathBuf {
    fixture.path("home/.config/oh-fx")
}

#[test]
fn applicable_symlinked_rules_are_omitted_without_exposing_their_target() {
    let fixture = Fixture::new();
    fixture.write("home/outside-secret.txt", b"DO_NOT_EXPOSE");
    fixture.link("../outside-secret.txt", "home/work/AGENTS.md");
    let context = fixture.gather("home/work", defaults());
    assert!(!visible(&context).contains("DO_NOT_EXPOSE"));
    assert!(visible(&context).contains("reason=\"symlinked rule file\""));
    assert!(context.notices[0].ends_with("; repair=replace the symlink with a regular file"));
}

#[test]
fn project_omission_notices_dedupe_and_explain_selection_and_emergency_size_failures() {
    let mut omissions = Omissions::default();
    let mut notices = Vec::new();
    omissions.add(
        Path::new("/work/deep/AGENTS.md"),
        OmissionReason::SelectionCap,
        &mut notices,
    );
    omissions.add(
        Path::new("/work/deep/AGENTS.md"),
        OmissionReason::SelectionCap,
        &mut notices,
    );
    omissions.add(
        Path::new("/work/AGENTS.md"),
        OmissionReason::Oversized,
        &mut notices,
    );
    assert_eq!(notices.len(), 2);
    assert!(notices[0].contains("action=omitted"));
    assert!(notices[0].contains("reason=selection cap"));
    assert!(notices[0].contains("reduce applicable AGENTS.md files"));
    assert!(notices[1].contains("reason=oversized rule file"));
    assert!(notices[1].contains(&format!("smaller than {EMERGENCY_CEILING_BYTES} bytes")));
    assert!(notices[1].contains("smaller than 67108864 bytes"));
    let mut out = String::new();
    omissions.render(&mut out, &mut notices);
    assert_eq!(
        out,
        "<project-rules-omitted from=\"/work/AGENTS.md\" reason=\"oversized rule file\" />\n\n<project-rules-omitted from=\"/work/deep/AGENTS.md\" reason=\"selection cap\" />"
    );
}

#[test]
fn home_source_loss_is_user_visible_while_non_ancestor_topology_stays_model_only() {
    let mut omissions = Omissions::default();
    let mut notices = Vec::new();
    omissions.add(
        Path::new("HOME"),
        OmissionReason::HomeUnavailable,
        &mut notices,
    );
    omissions.add(
        Path::new("/tmp/workspace"),
        OmissionReason::HomeOutsideWorkspace,
        &mut notices,
    );
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("set HOME"));
}

#[test]
fn project_omission_consequences_bound_oversized_sources_independently_of_the_content_limit() {
    let mut source = vec![b'a'; 256 * 1024];
    source[..21].copy_from_slice(b"https://example.test/");
    let tail = b"/REMOTE_URI_TAIL_SENTINEL";
    let start = source.len() - tail.len();
    source[start..].copy_from_slice(tail);
    let source = PathBuf::from(OsString::from_vec(source));
    let mut omissions = Omissions::default();
    let mut notices = Vec::new();
    omissions.add(&source, OmissionReason::UnsafeTarget, &mut notices);
    let mut out = String::new();
    omissions.render(&mut out, &mut notices);
    assert!(out.len() < 4096);
    assert!(!out.contains("REMOTE_URI_TAIL_SENTINEL"));
    assert!(out.contains("source_bytes=\"262144\""));
    assert!(out.contains("source_sha256=\""));
    assert!(out.contains("reason=\"unsafe target\""));
    assert!(out.starts_with(&format!(
        "<project-rules-omitted from=\"https://example.test/{}...\" source_bytes=\"262144\" source_sha256=\"",
        "a".repeat(256 - 21)
    )));
    assert_eq!(notices.len(), 1);
    assert!(notices[0].len() < 4096);
    assert!(!notices[0].contains("REMOTE_URI_TAIL_SENTINEL"));
    assert!(notices[0].contains("source_bytes=262144"));
    assert!(notices[0].contains("action=omitted"));
    assert!(notices[0].contains("reason=unsafe target"));
}

#[test]
fn project_omission_consequences_stay_bounded_across_many_sources() {
    let mut omissions = Omissions::default();
    let mut notices = Vec::new();
    for index in 0..128 {
        omissions.add(
            Path::new(&format!("https://example.test/resource/{index}")),
            OmissionReason::UnsafeTarget,
            &mut notices,
        );
    }
    let mut out = String::new();
    omissions.render(&mut out, &mut notices);
    assert!(out.len() < 64 * 1024);
    assert_eq!(out.matches("<project-rules-omitted from=").count(), 32);
    assert!(out.contains("<project-rules-omitted-summary omitted_count=\"96\" reasons=\"unsafe target:96\" records_sha256=\""));
    assert!(notices.len() <= 33);
    let last = notices.last().unwrap();
    assert!(last.starts_with(
        "[context] project instructions action=omitted summary: 96 additional records; reasons=\"unsafe target:96\" records_sha256="
    ));
    assert!(last.ends_with("; repair=review the listed omission reasons"));
}

#[test]
fn omission_summary_digests_match_upstream_records() {
    let mut omissions = Omissions::default();
    let mut notices = Vec::new();
    for index in 0..33 {
        omissions.add(
            Path::new(&format!("/s{index:02}")),
            OmissionReason::SelectionCap,
            &mut notices,
        );
    }
    omissions.add(
        Path::new("/w"),
        OmissionReason::HomeOutsideWorkspace,
        &mut notices,
    );
    let mut out = String::new();
    omissions.render(&mut out, &mut notices);
    assert!(out.ends_with(
        "<project-rules-omitted-summary omitted_count=\"2\" reasons=\"workspace is not below home:1, selection cap:1\" records_sha256=\"a8e72103c49b1f8f5b84c415\" />"
    ));
    assert_eq!(
        notices.last().unwrap(),
        "[context] project instructions action=omitted summary: 1 additional records; reasons=\"selection cap:1\" records_sha256=a8e72103c49b1f8f5b84c415; repair=review the listed omission reasons"
    );
}

fn file(path: PathBuf) -> ApplicableTarget {
    ApplicableTarget {
        path,
        kind: TargetKind::File,
    }
}

fn directory(path: PathBuf) -> ApplicableTarget {
    ApplicableTarget {
        path,
        kind: TargetKind::Directory,
    }
}

fn select(
    workspace: &Path,
    targets: &[ApplicableTarget],
    delivery: &DeliveryState,
) -> ProjectContext {
    select_applicable_project_context(workspace, targets, delivery, defaults())
}

#[test]
fn initial_gather_and_later_targets_order_global_ancestors_workspace_and_hidden_and_build_scopes() {
    let fixture = Fixture::new();
    fixture.write("home/.config/oh-fx/AGENTS.md", b"RULE_GLOBAL");
    fixture.write("home/projects/AGENTS.md", b"RULE_PARENT");
    fixture.write("home/projects/work/AGENTS.md", b"RULE_WORKSPACE");
    fixture.write("home/projects/work/.github/AGENTS.md", b"RULE_HIDDEN");
    fixture.write("home/projects/work/build/AGENTS.md", b"RULE_BUILD");
    fixture.write("home/projects/work/dist/AGENTS.md", b"RULE_UNRELATED");
    let hidden = fixture.write("home/projects/work/.github/workflows/ci.yml", b"");
    let build = fixture.write("home/projects/work/build/generated/out.zig", b"");
    let workspace = fixture.path("home/projects/work");
    let initial = fixture.gather("home/projects/work", defaults());
    let delivery = delivered(&initial);
    let later = select(&workspace, &[file(build), file(hidden)], &delivery);
    let text = format!("{}\n{}", visible(&initial), visible(&later));
    let order: Vec<usize> = [
        "RULE_GLOBAL",
        "RULE_PARENT",
        "RULE_WORKSPACE",
        "RULE_HIDDEN",
        "RULE_BUILD",
    ]
    .iter()
    .map(|needle| find(&text, needle))
    .collect();
    assert!(order.is_sorted(), "{text}");
    assert!(!text.contains("RULE_UNRELATED"));
    assert!(visible(&later).starts_with(&format!(
        "<project-instructions-guidance>\n{GUIDANCE}\n</project-instructions-guidance>\n\n<scoped-rules"
    )));
    assert_eq!(
        initial.delivered_sources.len() + later.delivered_sources.len(),
        5
    );
    assert_eq!(
        initial.evaluated_endpoints.len() + later.evaluated_endpoints.len(),
        3
    );
    assert_eq!(initial.evaluated_endpoints, [workspace]);
}

#[test]
fn initial_gather_records_a_contained_symlink_under_its_logical_source() {
    let fixture = Fixture::new();
    fixture.write("home/work/CLAUDE.md", b"LINKED_PROJECT_RULE");
    fixture.link("CLAUDE.md", "home/work/AGENTS.md");
    let context = fixture.gather("home/work", defaults());
    assert_eq!(
        context.delivered_sources,
        [fixture.path("home/work/AGENTS.md")]
    );
}

#[test]
fn scoped_selection_keeps_the_nearest_readable_cap_and_reports_unusable_and_capped_sources() {
    let fixture = Fixture::new();
    let oversized = vec![b'x'; defaults().file.effective_bytes() + 1];
    let mut relative = String::from("home/work");
    for index in 0..36 {
        let _ = write!(relative, "/level-{index:02}");
        let path = format!("{relative}/AGENTS.md");
        match index {
            2 => fixture.write(&path, b" \n "),
            3 => fixture.write(&path, &oversized),
            _ => fixture.write(&path, format!("RULE_LEVEL_{index:02}").as_bytes()),
        };
    }
    let target = fixture.write(&format!("{relative}/target.zig"), b"");
    let workspace = fixture.path("home/work");
    let context = select(&workspace, &[file(target)], &DeliveryState::default());
    let text = visible(&context);
    assert!(!text.contains("RULE_LEVEL_00"));
    assert!(!text.contains("RULE_LEVEL_01"));
    assert!(text.contains("RULE_LEVEL_04"));
    assert!(text.contains("RULE_LEVEL_35"));
    assert_eq!(text.matches("reason=\"selection cap\"").count(), 3);
    assert_eq!(text.matches("reason=\"oversized rule file\"").count(), 0);
    assert_eq!(context.delivered_sources.len(), 32);
}

#[test]
fn project_instruction_file_cap_bounds_every_large_candidate() {
    let fixture = Fixture::new();
    let mut large = vec![b'x'; 128 * 1024];
    large[..13].copy_from_slice(b"BOUNDED_RULE\n");
    let mut relative = String::from("work");
    for index in 0..8 {
        let _ = write!(relative, "/level-{index}");
        fixture.write(&format!("{relative}/AGENTS.md"), &large);
    }
    let target = fixture.write(&format!("{relative}/target.zig"), b"");
    let limits = with_file_limit(64, ContextLimitSource::CommandLine);
    let context = select_applicable_project_context(
        &fixture.path("work"),
        &[file(target)],
        &DeliveryState::default(),
        limits,
    );
    let text = visible(&context);
    assert_eq!(context.delivered_sources.len(), 8);
    assert_eq!(context.notices.len(), 8);
    assert_eq!(text.matches("BOUNDED_RULE").count(), 8);
    assert_eq!(
        text.matches("<context_limit name=\"project_instruction_file_bytes\"")
            .count(),
        8
    );
    assert!(context.notices[0].contains("observed=131072 bytes"));
}

#[test]
fn later_selection_adds_disjoint_scopes_once_and_does_not_repeat_their_common_ancestor() {
    let fixture = Fixture::new();
    fixture.write("home/work/src/AGENTS.md", b"RULE_COMMON");
    fixture.write("home/work/src/a/AGENTS.md", b"RULE_A");
    fixture.write("home/work/src/b/AGENTS.md", b"RULE_B");
    let target_a = fixture.write("home/work/src/a/a.zig", b"");
    let target_b = fixture.write("home/work/src/b/b.zig", b"");
    let workspace = fixture.path("home/work");
    let initial = fixture.gather("home/work", defaults());
    let mut delivery = delivered(&initial);
    let first = select(&workspace, &[file(target_a)], &delivery);
    assert!(visible(&first).contains("RULE_COMMON"));
    assert!(visible(&first).contains("RULE_A"));
    commit(&mut delivery, &first);
    let second = select(&workspace, &[file(target_b.clone())], &delivery);
    assert!(!visible(&second).contains("RULE_COMMON"));
    assert!(visible(&second).contains("RULE_B"));
    commit(&mut delivery, &second);
    let repeated = select(&workspace, &[file(target_b)], &delivery);
    assert_eq!(repeated, ProjectContext::default());
}

#[test]
fn later_selection_includes_the_target_directory_scope_only_for_directory_targets() {
    let fixture = Fixture::new();
    fixture.write("work/nested/AGENTS.md", b"RULE_NESTED_DIRECTORY");
    let workspace = fixture.path("work");
    let nested = fixture.path("work/nested");
    let delivery = DeliveryState::default();
    let as_directory = select(&workspace, &[directory(nested.clone())], &delivery);
    assert!(visible(&as_directory).contains("RULE_NESTED_DIRECTORY"));
    let as_file = select(&workspace, &[file(nested)], &delivery);
    assert!(!visible(&as_file).contains("RULE_NESTED_DIRECTORY"));
}

#[test]
fn equal_depth_disjoint_target_scopes_render_by_canonical_source_path() {
    let fixture = Fixture::new();
    fixture.write("work/a/AGENTS.md", b"RULE_A");
    fixture.write("work/z/AGENTS.md", b"RULE_Z");
    let target_a = fixture.write("work/a/a.zig", b"");
    let target_z = fixture.write("work/z/z.zig", b"");
    let context = select(
        &fixture.path("work"),
        &[file(target_z), file(target_a)],
        &DeliveryState::default(),
    );
    assert!(find(visible(&context), "RULE_A") < find(visible(&context), "RULE_Z"));
}

#[test]
fn later_external_targets_are_evaluated_without_context_or_non_workspace_rules() {
    let fixture = Fixture::new();
    fixture.write("work/AGENTS.md", b"RULE_WORKSPACE");
    fixture.write("external/AGENTS.md", b"RULE_MUST_NOT_ATTACH");
    let target = fixture.write("external/file.txt", b"external");
    let context = select(
        &fixture.path("work"),
        &[file(target)],
        &DeliveryState::default(),
    );
    assert_eq!(context.content, None);
    assert!(context.delivered_sources.is_empty());
    assert_eq!(context.evaluated_endpoints, [fixture.path("external")]);
    assert!(context.notices.is_empty());
}

#[test]
fn relative_and_empty_targets_are_reported_as_unsafe() {
    let fixture = Fixture::new();
    let context = select(
        &fixture.path("work"),
        &[
            file(PathBuf::from("relative.txt")),
            directory(PathBuf::new()),
        ],
        &DeliveryState::default(),
    );
    assert_eq!(
        visible(&context),
        "<project-rules-omitted from=\"(empty target)\" reason=\"unsafe target\" />\n\n<project-rules-omitted from=\"relative.txt\" reason=\"unsafe target\" />"
    );
    assert_eq!(context.notices.len(), 2);
}

fn delivered(context: &ProjectContext) -> DeliveryState {
    let mut state = DeliveryState::default();
    commit(&mut state, context);
    state
}

fn commit(state: &mut DeliveryState, context: &ProjectContext) {
    state
        .delivered_sources
        .extend(context.delivered_sources.iter().cloned());
    state
        .evaluated_endpoints
        .extend(context.evaluated_endpoints.iter().cloned());
}
