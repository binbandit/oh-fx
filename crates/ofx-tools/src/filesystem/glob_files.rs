use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use std::sync::Arc;

use ofx_contract::{
    CallDescription, CallPresentation, Concurrency, PathAccess, PreparedCall, Tool, ToolActivity,
    ToolOutput, ToolSpec, filesystem_access_denied_json, format_plain_action,
};
use ofx_text::sanitize_model_text_owned;
use ofx_workspace::{
    CandidatePaths, CandidateStats, CompileError, Discovery, DiscoveryOptions, MAX_PATTERN_BYTES,
    MAX_RELATIVE_PATH_BYTES, PathError, Pattern, Source, UntrackedFiles, basename, dirname,
    discover, path_contains_hidden_directory_component, path_inside,
    resolve_workspace_or_external_path, resolve_workspace_path, workspace_relative_path,
};

use super::{FilesystemContext, read_only_effect, render_candidate_notes, tool_spec};
use crate::tool_admission::admit_optional_path;
use crate::tool_args::{parse_arguments, required_string};
use crate::tool_runtime::BlockingCall;

const TOOL_NAME: &str = "glob_files";

const DESCRIPTION: &str = "Find file paths matching a glob pattern, with mode=count for exact path counts without listing entries. Paths may be workspace-relative or external using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy. When to use: locate files by name, extension, or directory pattern; narrow path or pattern if candidate caps appear. When NOT to use: search file contents, read files, run find, or count non-file concepts.";
const INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"pattern":{"type":"string","description":"Glob pattern to match, such as src/**/*.zig or *.md."},"path":{"type":"string","minLength":1,"description":"Optional search root relative to the workspace root, or an external path using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy. Omit this field to use the current directory; never send an empty string. Narrow it when possible."},"mode":{"type":"string","enum":["matches","count"],"description":"Use matches to return sample paths, or count to return an exact matching path count without listing entries."}},"required":["pattern"]}"#;
const PRESENTATION: CallPresentation = CallPresentation {
    activity: ToolActivity::List,
    action_label: "Matching",
    label_argument: "pattern",
    label_default: "pattern",
};

pub struct GlobFiles {
    spec: ToolSpec,
    context: Arc<FilesystemContext>,
}

impl GlobFiles {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            spec: tool_spec(TOOL_NAME, DESCRIPTION, INPUT_SCHEMA),
            context: Arc::new(FilesystemContext::new(workspace_root)),
        }
    }
}

impl Tool for GlobFiles {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        let decoded = GlobFilesArgs::decode(arguments);
        let description = CallDescription {
            title: format_plain_action(TOOL_NAME, &PRESENTATION, arguments),
            activity: PRESENTATION.activity,
            effect: read_only_effect(&decoded),
            concurrency: Concurrency::Parallel,
        };
        let context = Arc::clone(&self.context);
        Ok(BlockingCall::boxed(
            description,
            move |path_access| match decoded {
                Ok(arguments) => arguments.run(&context, path_access),
                Err(failure) => failure,
            },
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobMode {
    Matches,
    Count,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GlobFilesArgs {
    pattern: String,
    path: String,
    mode: GlobMode,
}

struct SearchRoot {
    absolute: PathBuf,
    is_directory: bool,
}

struct StaticGlobBase<'a> {
    base: &'a str,
    pattern: &'a str,
}

impl GlobFilesArgs {
    fn decode(args_json: &str) -> Result<Self, ToolOutput> {
        let arguments = parse_arguments(TOOL_NAME, args_json)?;
        let pattern = required_string(TOOL_NAME, &arguments, "pattern")?;
        let path = arguments
            .optional_string("path")
            .filter(|path| !path.is_empty())
            .unwrap_or(".");
        let mode = match arguments.optional_string("mode") {
            Some("count") => GlobMode::Count,
            _ => GlobMode::Matches,
        };
        Ok(Self {
            pattern,
            path: path.to_owned(),
            mode,
        })
    }

    fn run(&self, context: &FilesystemContext, path_access: PathAccess) -> ToolOutput {
        if let Err(failure) = admit_optional_path(TOOL_NAME, &context.workspace_root, &self.path) {
            return failure;
        }
        match self.execute(context, path_access) {
            Ok(text) => ToolOutput::success(text),
            Err(failure) => failure,
        }
    }

    fn execute(
        &self,
        context: &FilesystemContext,
        path_access: PathAccess,
    ) -> Result<String, ToolOutput> {
        self.search(context, &DiscoveryOptions::default(), path_access)
    }

    fn search(
        &self,
        context: &FilesystemContext,
        base_options: &DiscoveryOptions<'_>,
        path_access: PathAccess,
    ) -> Result<String, ToolOutput> {
        let Some((root, pattern)) = self.effective_root(context, path_access)? else {
            return Ok(self.format(&[], 0, false, &CandidateStats::default(), context));
        };
        let root_relative = workspace_relative_path(&context.workspace_root, &root.absolute);
        let root_relative = root_relative.as_os_str().as_bytes();
        let compiled =
            Pattern::compile(pattern.as_bytes()).map_err(|CompileError::PatternTooLong| {
                ToolOutput::failure(format!(
                    "glob_files field \"pattern\" must be at most {MAX_PATTERN_BYTES} bytes"
                ))
            })?;

        if !root.is_directory {
            let listed: Vec<Vec<u8>> = if compiled.matches_path(basename(root_relative)) {
                vec![root_relative.to_vec()]
            } else {
                Vec::new()
            };
            let stats = CandidateStats::default();
            return Ok(self.format(&listed, listed.len(), false, &stats, context));
        }

        let (candidates, stats) = self.candidates(context, &root, root_relative, base_options)?;
        let mut listed = Vec::new();
        let mut match_count = 0;
        let mut output_truncated = false;
        for candidate in candidates.iter() {
            if !compiled.matches_path(candidate) {
                continue;
            }
            match_count += 1;
            if self.mode == GlobMode::Count {
                continue;
            }
            if listed.len() >= context.max_list_entries {
                output_truncated = true;
                break;
            }
            listed.push(join_relative_search_path(root_relative, candidate));
        }
        Ok(self.format(&listed, match_count, output_truncated, &stats, context))
    }

    fn effective_root(
        &self,
        context: &FilesystemContext,
        path_access: PathAccess,
    ) -> Result<Option<(SearchRoot, &str)>, ToolOutput> {
        let requested_root = resolve_search_root(&context.workspace_root, &self.path, path_access)
            .map_err(|error| self.root_failure(error, &self.path))?;
        if !requested_root.is_directory {
            return Ok(Some((requested_root, &self.pattern)));
        }
        let static_base = extract_static_glob_base(&self.pattern);
        let root = resolve_static_base_root(&requested_root.absolute, static_base.base).map_err(
            |error| {
                if error.is_access_denied() {
                    access_denied(&requested_root.absolute, error)
                } else {
                    self.root_failure(error, &self.path)
                }
            },
        )?;
        Ok(root.map(|root| (root, static_base.pattern)))
    }

    fn candidates(
        &self,
        context: &FilesystemContext,
        root: &SearchRoot,
        root_relative: &[u8],
        base_options: &DiscoveryOptions<'_>,
    ) -> Result<(CandidatePaths, CandidateStats), ToolOutput> {
        let is_scoped_search_root = !root_relative.is_empty() && root_relative != b".";
        let options = DiscoveryOptions {
            ignored_names: context.ignored_list_entries,
            untracked: if is_scoped_search_root {
                UntrackedFiles::Include
            } else {
                base_options.untracked
            },
            force_fallback: base_options.force_fallback || is_scoped_search_root,
            include_hidden: base_options.include_hidden
                || path_contains_hidden_directory_component(root_relative)
                || path_contains_hidden_directory_component(self.pattern.as_bytes()),
            sort_paths: self.mode == GlobMode::Matches,
            ..*base_options
        };

        let discovered = discover(&root.absolute, &options);
        if !should_merge_root_untracked(root_relative, &discovered, &options) {
            return Ok((discovered.files, discovered.stats));
        }
        merge_root_untracked_candidates(&root.absolute, &options, discovered).map_err(|error| {
            let error = PathError::from(error);
            if error.is_access_denied() {
                access_denied(&root.absolute, error)
            } else {
                ToolOutput::failure(format!(
                    "Unable to discover glob candidates: {} ({error})",
                    root.absolute.display()
                ))
            }
        })
    }

    fn format(
        &self,
        listed: &[Vec<u8>],
        match_count: usize,
        output_truncated: bool,
        stats: &CandidateStats,
        context: &FilesystemContext,
    ) -> String {
        match self.mode {
            GlobMode::Count => format_count(&self.pattern, match_count, stats),
            GlobMode::Matches => format_matches(
                &self.pattern,
                listed,
                output_truncated,
                stats,
                context.max_list_entries,
            ),
        }
    }

    fn root_failure(&self, error: PathError, path: &str) -> ToolOutput {
        if error.is_access_denied() {
            return ToolOutput::failure(filesystem_access_denied_json(
                TOOL_NAME,
                path,
                &error.to_string(),
            ));
        }
        ToolOutput::failure(format!(
            "Unable to resolve glob search root: {} ({error})",
            self.path
        ))
    }
}

fn access_denied(path: &Path, error: PathError) -> ToolOutput {
    ToolOutput::failure(filesystem_access_denied_json(
        TOOL_NAME,
        &path.to_string_lossy(),
        &error.to_string(),
    ))
}

fn format_count(pattern: &str, match_count: usize, stats: &CandidateStats) -> String {
    format!(
        "[glob] count {match_count} matches for {pattern}\n{}",
        render_candidate_notes(stats)
    )
}

fn format_matches(
    pattern: &str,
    listed: &[Vec<u8>],
    output_truncated: bool,
    stats: &CandidateStats,
    max_list_entries: usize,
) -> String {
    if listed.is_empty() {
        return format!(
            "[glob] no matches for {pattern}\n{}",
            render_candidate_notes(stats)
        );
    }
    let mut out = format!("[glob] {} matches for {pattern}\n", listed.len()).into_bytes();
    for path in listed {
        out.extend_from_slice(b" - ");
        out.extend_from_slice(path);
        out.push(b'\n');
    }
    if output_truncated {
        out.extend_from_slice(
            format!("... truncated to first {max_list_entries} matches\n").as_bytes(),
        );
    }
    out.extend_from_slice(render_candidate_notes(stats).as_bytes());
    sanitize_model_text_owned(out)
}

fn resolve_search_root(
    workspace_root: &Path,
    requested: &str,
    path_access: PathAccess,
) -> Result<SearchRoot, PathError> {
    let absolute = resolve_workspace_or_external_path(workspace_root, requested)?;
    if path_access == PathAccess::WorkspaceOnly && !path_inside(workspace_root, &absolute) {
        return Err(PathError::PathOutsideWorkspace);
    }
    directory_or_file_root(absolute)
}

fn directory_or_file_root(absolute: PathBuf) -> Result<SearchRoot, PathError> {
    match fs::read_dir(&absolute) {
        Ok(_) => Ok(SearchRoot {
            absolute,
            is_directory: true,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => Ok(SearchRoot {
            absolute,
            is_directory: false,
        }),
        Err(error) => Err(error.into()),
    }
}

fn resolve_static_base_root(
    requested_root: &Path,
    static_base: &str,
) -> Result<Option<SearchRoot>, PathError> {
    if static_base.is_empty() || static_base == "." {
        return Ok(Some(SearchRoot {
            absolute: requested_root.to_path_buf(),
            is_directory: true,
        }));
    }
    let absolute = match resolve_workspace_path(requested_root, static_base) {
        Ok(absolute) => absolute,
        Err(PathError::FileNotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    let root = directory_or_file_root(absolute)?;
    Ok(root.is_directory.then_some(root))
}

fn extract_static_glob_base(pattern: &str) -> StaticGlobBase<'_> {
    let Some(first_glob) = pattern.find(['*', '?', '[', '{']) else {
        let bytes = pattern.as_bytes();
        let Some(base) = dirname(bytes) else {
            return StaticGlobBase { base: "", pattern };
        };
        let name_end = pattern.trim_end_matches('/').len();
        let name_start = name_end - basename(bytes).len();
        return StaticGlobBase {
            base: &pattern[..base.len()],
            pattern: &pattern[name_start..name_end],
        };
    };
    let static_prefix = &pattern[..first_glob];
    match static_prefix.rfind(['/', '\\']) {
        None => StaticGlobBase { base: "", pattern },
        Some(0) => StaticGlobBase {
            base: &static_prefix[..1],
            pattern: &pattern[1..],
        },
        Some(last_separator) => StaticGlobBase {
            base: &static_prefix[..last_separator],
            pattern: &pattern[last_separator + 1..],
        },
    }
}

fn join_relative_search_path(root_relative: &[u8], child_relative: &[u8]) -> Vec<u8> {
    if root_relative.is_empty() || root_relative == b"." {
        return child_relative.to_vec();
    }
    let separator: &[u8] = if root_relative.ends_with(b"/") {
        b""
    } else {
        b"/"
    };
    [root_relative, separator, child_relative].concat()
}

fn should_merge_root_untracked(
    root_relative: &[u8],
    discovered: &Discovery,
    options: &DiscoveryOptions<'_>,
) -> bool {
    discovered.source == Source::Git
        && options.untracked != UntrackedFiles::Include
        && !options.force_fallback
        && (root_relative.is_empty() || root_relative == b".")
}

fn merge_root_untracked_candidates(
    absolute_root: &Path,
    options: &DiscoveryOptions<'_>,
    discovered: Discovery,
) -> io::Result<(CandidatePaths, CandidateStats)> {
    let Discovery {
        mut files,
        mut stats,
        ..
    } = discovered;
    let Ok(entries) = fs::read_dir(absolute_root) else {
        return Ok((files, stats));
    };
    let mut root_names: Vec<&[u8]> = files.iter().filter(|path| !path.contains(&b'/')).collect();
    root_names.sort_unstable();
    let mut added: Vec<OsString> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() && !file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name_bytes = name.as_bytes();
        if options
            .ignored_names
            .iter()
            .any(|ignored| ignored.as_bytes() == name_bytes)
            || root_names.binary_search(&name_bytes).is_ok()
        {
            continue;
        }
        if name_bytes.len() > MAX_RELATIVE_PATH_BYTES {
            stats.skipped_overlong += 1;
            continue;
        }
        if files.len() + added.len() >= options.candidate_cap {
            stats.incomplete = true;
            break;
        }
        added.push(name);
    }
    for name in &added {
        files.push(name.as_bytes());
    }
    if options.sort_paths {
        files.sort();
    }
    Ok((files, stats))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use ofx_contract::ToolEffect;
    use tempfile::TempDir;

    use super::*;
    use crate::filesystem::tests::{run_git, run_tool, run_tool_with};

    struct Workspace {
        _temp: TempDir,
        root: PathBuf,
    }

    impl Workspace {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let root = fs::canonicalize(temp.path()).unwrap();
            Self { _temp: temp, root }
        }

        fn write(&self, relative: &str, content: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            path
        }

        fn context(&self) -> FilesystemContext {
            FilesystemContext::new(&self.root)
        }

        fn glob(&self, pattern: &str, path: Option<&str>) -> Result<String, ToolOutput> {
            let mut arguments = serde_json::json!({ "pattern": pattern });
            if let Some(path) = path {
                arguments["path"] = serde_json::Value::from(path);
            }
            GlobFilesArgs::decode(&arguments.to_string())?
                .execute(&self.context(), PathAccess::WorkspaceOrExternal)
        }

        fn numbered_files(&self, count: usize) {
            for index in 0..count {
                self.write(&format!("many/file-{index:04}.txt"), "x\n");
            }
        }
    }

    fn count_emitted_result_lines(body: &str) -> usize {
        body.lines().filter(|line| line.starts_with(" - ")).count()
    }

    fn incomplete_candidates() -> CandidateStats {
        CandidateStats {
            incomplete: true,
            ..CandidateStats::default()
        }
    }

    #[test]
    fn glob_files_decodes_invalid_argument_shapes_as_failures() {
        let cases = [
            ("{", "glob_files arguments must be valid JSON"),
            ("[]", "glob_files arguments must be an object"),
            (
                "{\"path\":\".\"}",
                "glob_files requires string field \"pattern\"",
            ),
            (
                "{\"pattern\":1}",
                "glob_files field \"pattern\" must be a string",
            ),
            (
                "{\"pattern\":\"*\",\"pattern\":\"*\"}",
                "glob_files arguments must be valid JSON",
            ),
            (
                "{\"pattern\":1e400}",
                "glob_files field \"pattern\" must be a string",
            ),
        ];
        for (json, reason) in cases {
            assert_eq!(
                GlobFilesArgs::decode(json),
                Err(ToolOutput::failure(reason)),
                "{json}"
            );
        }
    }

    #[test]
    fn glob_files_decodes_omitted_path_and_valid_input() {
        let cases = [
            ("{\"pattern\":\"*.zig\"}", "."),
            ("{\"pattern\":\"*.zig\",\"path\":\"\"}", "."),
            ("{\"pattern\":\"*.zig\",\"path\":\"src\"}", "src"),
            ("{\"pattern\":\"*.zig\",\"path\":1}", "."),
        ];
        for (json, path) in cases {
            let decoded = GlobFilesArgs::decode(json).unwrap();
            assert_eq!(decoded.pattern, "*.zig");
            assert_eq!(decoded.path, path);
            assert_eq!(decoded.mode, GlobMode::Matches);
        }
        let count = GlobFilesArgs::decode("{\"pattern\":\"*\",\"mode\":\"count\"}").unwrap();
        assert_eq!(count.mode, GlobMode::Count);
    }

    #[test]
    fn glob_files_validate_preserves_active_raw_pattern_and_path_values() {
        let empty = GlobFilesArgs::decode("{\"pattern\":\" \\t\\n \",\"path\":\"   \"}").unwrap();
        assert_eq!(empty.pattern, " \t\n ");
        assert_eq!(empty.path, "   ");
        let valid =
            GlobFilesArgs::decode("{\"pattern\":\"  **/*.zig  \",\"path\":\"  src  \"}").unwrap();
        assert_eq!(valid.pattern, "  **/*.zig  ");
        assert_eq!(valid.path, "  src  ");
    }

    #[test]
    fn glob_files_reports_overlong_patterns_during_matching() {
        let workspace = Workspace::new();
        let pattern = "*".repeat(MAX_PATTERN_BYTES + 1);

        assert_eq!(
            workspace.glob(&pattern, Some(".")),
            Err(ToolOutput::failure(format!(
                "glob_files field \"pattern\" must be at most {MAX_PATTERN_BYTES} bytes"
            )))
        );
    }

    #[test]
    fn glob_files_regular_file_search_root_matches_basename_only() {
        let workspace = Workspace::new();
        let file_path = workspace.write("single.txt", "one\n");
        let file_path = file_path.to_str().unwrap();

        assert_eq!(
            workspace.glob("*.txt", Some(file_path)).unwrap(),
            "[glob] 1 matches for *.txt\n - single.txt\n"
        );
        assert_eq!(
            workspace.glob("dir/*.txt", Some(file_path)).unwrap(),
            "[glob] no matches for dir/*.txt\n"
        );
    }

    #[test]
    fn glob_files_regular_file_root_follows_active_basename_behavior_inside_ignored_directory() {
        let workspace = Workspace::new();
        let ignored_file = workspace.write("node_modules/pkg/file.zig", "ignored\n");

        assert_eq!(
            workspace
                .glob("*.zig", Some(ignored_file.to_str().unwrap()))
                .unwrap(),
            "[glob] 1 matches for *.zig\n - node_modules/pkg/file.zig\n"
        );
    }

    #[test]
    fn glob_files_explicit_ignored_directory_root_follows_active_behavior() {
        let workspace = Workspace::new();
        workspace.write("node_modules/pkg/file.zig", "ignored\n");

        assert_eq!(
            workspace.glob("**/*.zig", Some("node_modules")).unwrap(),
            "[glob] 1 matches for **/*.zig\n - node_modules/pkg/file.zig\n"
        );
    }

    #[test]
    fn glob_files_resolver_error_for_missing_path_returns_failure_result() {
        let workspace = Workspace::new();

        assert_eq!(
            workspace.glob("*.zig", Some("missing")),
            Err(ToolOutput::failure(
                "Unable to resolve glob search root: missing (FileNotFound)"
            ))
        );
    }

    #[test]
    fn glob_files_permission_denied_directory_returns_structured_recovery() {
        let workspace = Workspace::new();
        let blocked = workspace.root.join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();

        let result = workspace.glob("**/*.zig", Some(blocked.to_str().unwrap()));
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        let Err(failure) = result else {
            return;
        };

        let body = failure.content;
        assert!(body.starts_with("{\"error\":{\"type\":\"tool_execution_failed\""));
        assert!(body.contains("\"tool_name\":\"glob_files\""));
        assert!(body.contains(blocked.to_str().unwrap()));
        assert!(body.contains("AccessDenied"));
        assert!(body.contains("symlink"));
    }

    #[test]
    fn glob_files_formats_zero_one_and_multiple_results() {
        let workspace = Workspace::new();
        workspace.write("one.txt", "one\n");
        workspace.write("two.txt", "two\n");

        assert_eq!(
            workspace.glob("*.zig", None).unwrap(),
            "[glob] no matches for *.zig\n"
        );
        assert_eq!(
            workspace.glob("one.txt", None).unwrap(),
            "[glob] 1 matches for one.txt\n - one.txt\n"
        );
        let multiple = workspace.glob("*.txt", None).unwrap();
        assert!(multiple.starts_with("[glob] 2 matches for *.txt\n"));
        assert!(multiple.contains(" - one.txt\n"));
        assert!(multiple.contains(" - two.txt\n"));
    }

    #[test]
    fn glob_files_truncates_output_to_active_max_list_entries() {
        let workspace = Workspace::new();
        workspace.numbered_files(150);

        let result = workspace.glob("**/*.txt", None).unwrap();

        assert!(result.starts_with("[glob] 100 matches for **/*.txt\n"));
        assert_eq!(count_emitted_result_lines(&result), 100);
        assert!(result.contains("... truncated to first 100 matches\n"));
        assert!(result.contains(" - many/file-0000.txt\n"));
    }

    #[test]
    fn glob_files_count_mode_reports_exact_matches_without_rendering_entries() {
        let workspace = Workspace::new();
        workspace.numbered_files(150);

        let result = GlobFilesArgs::decode("{\"pattern\":\"**/*.txt\",\"mode\":\"count\"}")
            .unwrap()
            .execute(&workspace.context(), PathAccess::WorkspaceOrExternal)
            .unwrap();

        assert_eq!(result, "[glob] count 150 matches for **/*.txt\n");
    }

    #[test]
    fn glob_files_formatter_uses_active_truncation_wording() {
        let listed = [b"a.txt".to_vec(), b"b.txt".to_vec()];
        assert_eq!(
            format_matches("*.txt", &listed, true, &CandidateStats::default(), 2),
            "[glob] 2 matches for *.txt\n - a.txt\n - b.txt\n... truncated to first 2 matches\n"
        );
    }

    #[test]
    fn glob_files_path_narrowing_applies_before_candidate_cap() {
        let workspace = Workspace::new();
        workspace.write("aaa/outside.txt", "outside\n");
        workspace.write("src/core/workspace/target.zig", "target\n");
        let mut context = workspace.context();
        context.max_list_entries = 10;
        let args = GlobFilesArgs {
            pattern: "target.zig".into(),
            path: "src/core/workspace".into(),
            mode: GlobMode::Matches,
        };
        let options = DiscoveryOptions {
            candidate_cap: 1,
            force_fallback: true,
            ..DiscoveryOptions::default()
        };

        assert_eq!(
            args.search(&context, &options, PathAccess::WorkspaceOrExternal)
                .unwrap(),
            "[glob] 1 matches for target.zig\n - src/core/workspace/target.zig\n"
        );
    }

    #[test]
    fn glob_files_extracts_static_base_before_candidate_cap() {
        let workspace = Workspace::new();
        workspace.write("aaa/outside.zig", "outside\n");
        workspace.write("src/tools/target.zig", "target\n");
        let mut context = workspace.context();
        context.max_list_entries = 10;
        let args = GlobFilesArgs {
            pattern: "src/tools/**/*.zig".into(),
            path: ".".into(),
            mode: GlobMode::Matches,
        };
        let options = DiscoveryOptions {
            candidate_cap: 1,
            force_fallback: true,
            ..DiscoveryOptions::default()
        };

        assert_eq!(
            args.search(&context, &options, PathAccess::WorkspaceOrExternal)
                .unwrap(),
            "[glob] 1 matches for src/tools/**/*.zig\n - src/tools/target.zig\n"
        );
    }

    #[test]
    fn glob_files_root_git_discovery_includes_untracked_files() {
        let workspace = Workspace::new();
        let git = |args: &[&str]| run_git(&workspace.root, args);
        if !git(&["init", "--quiet"]) {
            return;
        }
        workspace.write("tracked.txt", "tracked\n");
        if !git(&["add", "tracked.txt"]) {
            return;
        }
        workspace.write("untracked-target.txt", "untracked\n");

        assert_eq!(
            workspace.glob("untracked-*.txt", Some(".")).unwrap(),
            "[glob] 1 matches for untracked-*.txt\n - untracked-target.txt\n"
        );
    }

    #[test]
    fn glob_files_root_git_discovery_merges_untracked_root_files_once_in_both_modes() {
        let workspace = Workspace::new();
        let git = |args: &[&str]| run_git(&workspace.root, args);
        if !git(&["init", "--quiet"]) {
            return;
        }
        workspace.write("tracked.txt", "tracked\n");
        workspace.write("nested/tracked.txt", "nested\n");
        if !git(&["add", "tracked.txt", "nested/tracked.txt"]) {
            return;
        }
        workspace.write("b-untracked.txt", "untracked\n");
        workspace.write("a-untracked.txt", "untracked\n");
        workspace.write("nested/untracked.txt", "nested untracked\n");

        assert_eq!(
            workspace.glob("*.txt", None).unwrap(),
            "[glob] 4 matches for *.txt\n - a-untracked.txt\n - b-untracked.txt\n - nested/tracked.txt\n - tracked.txt\n"
        );
        let count = GlobFilesArgs::decode("{\"pattern\":\"*.txt\",\"mode\":\"count\"}")
            .unwrap()
            .execute(&workspace.context(), PathAccess::WorkspaceOrExternal)
            .unwrap();
        assert_eq!(count, "[glob] count 4 matches for *.txt\n");
    }

    #[test]
    fn glob_files_static_base_extraction_handles_literal_and_wildcard_patterns() {
        let cases = [
            ("src/core/**/*.zig", "src/core", "**/*.zig"),
            ("src/main.zig", "src", "main.zig"),
            ("*.zig", "", "*.zig"),
            ("/abs/*.txt", "/abs", "*.txt"),
            ("/*.txt", "/", "*.txt"),
            ("src/dir/", "src", "dir"),
        ];
        for (pattern, base, rest) in cases {
            let extracted = extract_static_glob_base(pattern);
            assert_eq!(extracted.base, base, "{pattern}");
            assert_eq!(extracted.pattern, rest, "{pattern}");
        }
    }

    #[test]
    fn glob_files_finds_match_beyond_former_traversal_cap() {
        let workspace = Workspace::new();
        workspace.numbered_files(2050);
        workspace.write("zzzz/target.zig", "target\n");

        let result = workspace.glob("**/*.zig", None).unwrap();

        assert!(result.contains("zzzz/target.zig"));
        assert!(!result.contains("traversal cap"));
    }

    #[test]
    fn glob_files_distinguishes_candidate_cap_from_output_cap() {
        let listed = [b"a.txt".to_vec(), b"b.txt".to_vec()];
        let body = format_matches("*.txt", &listed, true, &incomplete_candidates(), 2);

        assert!(body.contains("... truncated to first 2 matches\n"));
        assert!(body.contains("candidate cap 100000 reached"));
    }

    #[test]
    fn glob_files_allows_external_absolute_path_search_roots() {
        let workspace = Workspace::new();
        let inside = workspace.root.join("workspace");
        fs::create_dir(&inside).unwrap();
        let external_file = workspace.write("external/outside.txt", "outside\n");
        let context = FilesystemContext::new(&inside);
        let args = GlobFilesArgs {
            pattern: "*.txt".into(),
            path: workspace.root.join("external").to_str().unwrap().into(),
            mode: GlobMode::Matches,
        };

        assert_eq!(
            args.execute(&context, PathAccess::WorkspaceOrExternal)
                .unwrap(),
            format!(
                "[glob] 1 matches for *.txt\n - {}\n",
                external_file.display()
            )
        );
    }

    #[test]
    fn glob_files_keeps_workspace_only_calls_inside_the_workspace() {
        let workspace = Workspace::new();
        let inside = workspace.root.join("workspace");
        fs::create_dir(&inside).unwrap();
        let external_file = workspace.write("external/outside.txt", "outside\n");
        let tool = GlobFiles::new(&inside);
        let external = workspace.root.join("external");
        let arguments = serde_json::json!({ "pattern": "*.txt", "path": external }).to_string();

        let (_, held) = run_tool_with(&tool, &arguments, PathAccess::WorkspaceOnly);
        assert_eq!(
            held,
            ToolOutput::failure(format!(
                "Unable to resolve glob search root: {} (PathOutsideWorkspace)",
                external.display()
            ))
        );
        let (_, approved) = run_tool_with(&tool, &arguments, PathAccess::WorkspaceOrExternal);
        assert_eq!(
            approved,
            ToolOutput::success(format!(
                "[glob] 1 matches for *.txt\n - {}\n",
                external_file.display()
            ))
        );
    }

    #[test]
    fn glob_files_pattern_static_base_cannot_escape_the_approved_search_root() {
        let workspace = Workspace::new();
        let inside = workspace.root.join("workspace");
        fs::create_dir(&inside).unwrap();
        workspace.write("external/outside.txt", "outside\n");
        let context = FilesystemContext::new(&inside);
        let absolute_pattern = format!("{}/*.txt", workspace.root.join("external").display());

        for pattern in ["../external/*.txt", absolute_pattern.as_str()] {
            let args = GlobFilesArgs {
                pattern: pattern.into(),
                path: ".".into(),
                mode: GlobMode::Matches,
            };
            let failure = args
                .execute(&context, PathAccess::WorkspaceOrExternal)
                .unwrap_err();
            assert!(
                failure.content.contains("PathOutsideWorkspace"),
                "{pattern}"
            );
        }
    }

    #[test]
    fn glob_files_describes_read_only_list_calls_and_admits_the_search_root() {
        let workspace = Workspace::new();
        workspace.write("a.txt", "a\n");
        let tool = GlobFiles::new(&workspace.root);

        let (description, output) = run_tool(&tool, "{\"pattern\":\"*.txt\"}");
        assert_eq!(
            description,
            CallDescription {
                title: "Matching *.txt".to_owned(),
                activity: ToolActivity::List,
                effect: ToolEffect::ReadOnly,
                concurrency: Concurrency::Parallel,
            }
        );
        assert_eq!(
            output,
            ToolOutput::success("[glob] 1 matches for *.txt\n - a.txt\n")
        );

        let (description, output) = run_tool(&tool, "{\"pattern\":\"*\",\"path\":\"nope\"}");
        assert_eq!(description.title, "Matching *");
        assert_eq!(description.effect, ToolEffect::ReadOnly);
        assert_eq!(output, ToolOutput::failure("Path not found: nope"));

        let (description, output) = run_tool(&tool, "{\"path\":\".\"}");
        assert_eq!(description.title, "Matching pattern");
        assert_eq!(description.effect, ToolEffect::None);
        assert_eq!(
            output,
            ToolOutput::failure("glob_files requires string field \"pattern\"")
        );
    }
}
