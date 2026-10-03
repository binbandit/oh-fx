use std::collections::HashSet;
use std::fs;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use std::sync::Arc;

use memchr::{memchr, memchr_iter};
use ofx_contract::{
    CallPresentation, PathAccess, PreparedCall, Tool, ToolActivity, ToolOutput, ToolSpec,
    filesystem_access_denied_json, plain_description,
};
use ofx_text::sanitize_model_text_owned;
use ofx_workspace::{
    COLLECTION_CAP, CompileError, CountResult, GrepQuery, GrepResult, MAX_PATTERN_BYTES, Match,
    OUTPUT_CAP, PathError, Pattern, TruncatedReason, collect_directory_matches,
    collect_regular_file_root, count_directory_matches, count_regular_file_root, path_inside,
    read_model_safe, resolve_workspace_or_external_path, workspace_relative_path,
};

use super::{FilesystemContext, read_only_effect, render_candidate_notes, tool_spec};
use crate::tool_admission::admit_optional_path;
use crate::tool_args::{optional_integer, parse_arguments, required_string};
use crate::tool_runtime::BlockingCall;

const TOOL_NAME: &str = "grep_files";
const CONTEXT_LINES_CAP: usize = 5;

const DESCRIPTION: &str = "Search text files for a literal substring, optionally narrowed by path/include, with output modes for matching lines, files-with-matches, or counts plus head_limit/offset pagination and bounded context_lines for matches mode. Paths may be workspace-relative or external using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy. Use include as the type/path filter, such as *.zig. When to use: find exact symbols, strings, TODOs, or usage sites. When NOT to use: regex is not supported; avoid unknown-concept exploration, filename lookup, known-path reads, and shell grep; do not repeat the same or equivalent search after a caller search only finds a definition.";
const INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"pattern":{"type":"string","description":"Literal plain-text pattern to search for."},"path":{"type":"string","minLength":1,"description":"Optional search root relative to the workspace root, or an external path using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy. Omit this field to use the current directory; never send an empty string. Narrow it when possible."},"include":{"type":"string","description":"Optional glob pattern applied to candidate file paths before reading files, such as *.zig or src/**/*.ts."},"case_insensitive":{"type":"boolean","description":"Search case-insensitively when true."},"mode":{"type":"string","enum":["matches","files_with_matches","count"],"description":"Use matches for line matches, files_with_matches for unique matching paths, or count for exact matching-line and matching-file counts."},"head_limit":{"type":"integer","description":"Optional positive maximum results to return for matches or files_with_matches. Defaults to the normal output cap."},"offset":{"type":"integer","description":"Optional zero-based result offset for matches or files_with_matches pagination. Defaults to 0."},"context_lines":{"type":"integer","description":"Optional non-negative number of lines before and after each emitted match in matches mode. Bounded by the tool."}},"required":["pattern"]}"#;
const PRESENTATION: CallPresentation = CallPresentation {
    activity: ToolActivity::Read,
    action_label: "Searching",
    completed_label: "Searched",
    label_argument: "pattern",
    label_default: "pattern",
};

pub struct GrepFiles {
    spec: ToolSpec,
    context: Arc<FilesystemContext>,
}

impl GrepFiles {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            spec: tool_spec(TOOL_NAME, DESCRIPTION, INPUT_SCHEMA),
            context: Arc::new(FilesystemContext::new(workspace_root)),
        }
    }
}

impl Tool for GrepFiles {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        let decoded = GrepFilesArgs::decode(arguments);
        let description = plain_description(
            TOOL_NAME,
            &PRESENTATION,
            arguments,
            read_only_effect(&decoded),
        );
        let context = Arc::clone(&self.context);
        Ok(BlockingCall::boxed(
            description,
            move |path_access| match decoded {
                Ok(arguments) => arguments.run(&context, &path_access),
                Err(failure) => failure,
            },
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrepMode {
    Matches,
    FilesWithMatches,
    Count,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GrepFilesArgs {
    pattern: String,
    path: String,
    include: Option<String>,
    case_insensitive: bool,
    mode: GrepMode,
    head_limit: usize,
    offset: usize,
    context_lines: usize,
}

enum SearchOutcome {
    Matches(GrepResult),
    Count(CountResult),
}

struct Page {
    start: usize,
    end: usize,
    total: usize,
}

impl Page {
    fn new(offset: usize, limit: usize, total: usize) -> Self {
        let start = offset.min(total);
        Self {
            start,
            end: (start + limit).min(total),
            total,
        }
    }

    fn emitted(&self) -> usize {
        self.end - self.start
    }

    fn header_suffix(&self) -> String {
        if self.start == 0 && self.end == self.total {
            String::new()
        } else {
            format!(
                " (showing {}-{} of {})",
                self.start + 1,
                self.end,
                self.total
            )
        }
    }
}

impl GrepFilesArgs {
    fn decode(args_json: &str) -> Result<Self, ToolOutput> {
        let arguments = parse_arguments(TOOL_NAME, args_json)?;
        let pattern = required_string(TOOL_NAME, &arguments, "pattern")?;
        let path = arguments
            .optional_string("path")
            .filter(|path| !path.is_empty())
            .unwrap_or(".");
        let include = arguments.optional_string("include").map(ToOwned::to_owned);
        let case_insensitive = arguments.optional_bool("case_insensitive").unwrap_or(false);
        let mode = match arguments.optional_string("mode") {
            Some("count") => GrepMode::Count,
            Some("files_with_matches") => GrepMode::FilesWithMatches,
            _ => GrepMode::Matches,
        };
        let head_limit = optional_integer(TOOL_NAME, &arguments, "head_limit", 1, "positive")?
            .map_or(OUTPUT_CAP, |limit| limit.min(OUTPUT_CAP));
        let offset =
            optional_integer(TOOL_NAME, &arguments, "offset", 0, "non-negative")?.unwrap_or(0);
        let context_lines =
            optional_integer(TOOL_NAME, &arguments, "context_lines", 0, "non-negative")?
                .map_or(0, |lines| lines.min(CONTEXT_LINES_CAP));
        Ok(Self {
            pattern,
            path: path.to_owned(),
            include,
            case_insensitive,
            mode,
            head_limit,
            offset,
            context_lines,
        })
    }

    fn run(&self, context: &FilesystemContext, path_access: &PathAccess) -> ToolOutput {
        if let Err(failure) = admit_optional_path(TOOL_NAME, &context.workspace_root, &self.path) {
            return failure;
        }
        match self.execute(context, path_access) {
            Ok(text) => ToolOutput::success(text),
            Err(failure) => failure,
        }
    }

    fn resolve_root(
        &self,
        context: &FilesystemContext,
        path_access: &PathAccess,
    ) -> Result<PathBuf, ToolOutput> {
        let failure = |error: PathError| {
            if error.is_access_denied() {
                access_denied(&self.path, error)
            } else {
                ToolOutput::failure(format!(
                    "Unable to resolve grep search root: {} ({error})",
                    self.path
                ))
            }
        };
        let root = resolve_workspace_or_external_path(&context.workspace_root, &self.path)
            .map_err(failure)?;
        if path_access
            .confining_root(&context.workspace_root)
            .is_some_and(|confining| !path_inside(confining, &root))
        {
            return Err(failure(PathError::PathOutsideWorkspace));
        }
        Ok(root)
    }

    fn execute(
        &self,
        context: &FilesystemContext,
        path_access: &PathAccess,
    ) -> Result<String, ToolOutput> {
        let absolute_root = self.resolve_root(context, path_access)?;
        let include = self
            .include
            .as_deref()
            .map(|include| Pattern::compile(include.as_bytes()))
            .transpose()
            .map_err(|CompileError::PatternTooLong| {
                ToolOutput::failure(format!(
                    "grep_files field \"include\" must be at most {MAX_PATTERN_BYTES} bytes"
                ))
            })?;
        let absolute_text = absolute_root.to_string_lossy().into_owned();
        let root_type = fs::metadata(&absolute_root)
            .map_err(|error| {
                let error = PathError::from(error);
                if error.is_access_denied() {
                    access_denied(&absolute_text, error)
                } else {
                    ToolOutput::failure(format!(
                        "Unable to stat grep search root: {absolute_text} ({error})"
                    ))
                }
            })?
            .file_type();

        let query = GrepQuery {
            workspace_root: &context.workspace_root,
            pattern: &self.pattern,
            case_insensitive: self.case_insensitive,
            include: include.as_ref(),
            ignored_names: context.ignored_list_entries,
        };
        let outcome = if root_type.is_dir() {
            if self.mode == GrepMode::Count {
                SearchOutcome::Count(count_directory_matches(&query, &absolute_root))
            } else {
                SearchOutcome::Matches(collect_directory_matches(&query, &absolute_root))
            }
        } else if root_type.is_file() {
            let scan_failure = |error: PathError| {
                if error.is_access_denied() {
                    access_denied(&absolute_text, error)
                } else {
                    ToolOutput::failure(format!(
                        "Unable to scan grep file root: {absolute_text} ({error})"
                    ))
                }
            };
            if self.mode == GrepMode::Count {
                SearchOutcome::Count(
                    count_regular_file_root(&query, &absolute_root).map_err(scan_failure)?,
                )
            } else {
                SearchOutcome::Matches(
                    collect_regular_file_root(&query, &absolute_root).map_err(scan_failure)?,
                )
            }
        } else {
            return Err(ToolOutput::failure(format!(
                "Not a regular file or directory: {absolute_text}"
            )));
        };

        Ok(match outcome {
            SearchOutcome::Count(count) => format_count(&self.pattern, &count),
            SearchOutcome::Matches(result) if self.mode == GrepMode::FilesWithMatches => {
                format_files_with_matches(
                    &context.workspace_root,
                    &self.pattern,
                    &result,
                    self.offset,
                    self.head_limit.min(context.max_list_entries),
                )
            }
            SearchOutcome::Matches(result) => format_matches(
                &context.workspace_root,
                &self.pattern,
                &result,
                &MatchLayout {
                    head_limit: self.head_limit,
                    offset: self.offset,
                    context_lines: self.context_lines,
                    max_list_entries: context.max_list_entries,
                    max_line_len: context.max_read_file_line_len,
                },
            ),
        })
    }
}

fn access_denied(path: &str, error: PathError) -> ToolOutput {
    ToolOutput::failure(filesystem_access_denied_json(
        TOOL_NAME,
        path,
        &error.to_string(),
    ))
}

struct MatchLayout {
    head_limit: usize,
    offset: usize,
    context_lines: usize,
    max_list_entries: usize,
    max_line_len: usize,
}

fn format_matches(
    workspace_root: &Path,
    pattern: &str,
    result: &GrepResult,
    layout: &MatchLayout,
) -> String {
    let matches = &result.matches;
    let page = Page::new(
        layout.offset,
        layout.head_limit.min(layout.max_list_entries),
        matches.len(),
    );
    let mut out = Vec::new();
    if matches.is_empty() {
        out.extend_from_slice(format!("[grep] no matches for {pattern}\n").as_bytes());
    } else if page.emitted() == 0 {
        out.extend_from_slice(
            format!(
                "[grep] no matches for {pattern} at offset {} ({} total matches)\n",
                layout.offset,
                matches.len()
            )
            .as_bytes(),
        );
    } else {
        out.extend_from_slice(
            format!(
                "[grep] {} matches for {pattern}{}\n",
                page.emitted(),
                page.header_suffix()
            )
            .as_bytes(),
        );
        let mut context = ContextLines::default();
        for found in &matches[page.start..page.end] {
            let path = display_path(workspace_root, &found.absolute_path);
            let lines = if layout.context_lines > 0 {
                context.for_file(&found.read_path)
            } else {
                None
            };
            write_match_with_context(&mut out, found, &path, lines.as_ref(), layout);
        }
    }
    if page.end < matches.len() {
        out.extend_from_slice(
            format!(
                "... more matches available; use offset {} to continue\n",
                page.end
            )
            .as_bytes(),
        );
    }
    write_grep_result_notes(&mut out, result);
    sanitize_model_text_owned(out)
}

#[derive(Default)]
struct ContextLines {
    path: Option<PathBuf>,
    readable: bool,
    content: Vec<u8>,
    line_starts: Vec<usize>,
}

struct FileLines<'a> {
    content: &'a [u8],
    line_starts: &'a [usize],
}

impl ContextLines {
    fn for_file(&mut self, read_path: &Path) -> Option<FileLines<'_>> {
        if self.path.as_deref() != Some(read_path) {
            self.path = Some(read_path.to_path_buf());
            self.readable = read_model_safe(read_path, &mut self.content).unwrap_or(false);
            self.line_starts.clear();
            if self.readable && !self.content.is_empty() {
                let content_len = self.content.len();
                self.line_starts.push(0);
                self.line_starts.extend(
                    memchr_iter(b'\n', &self.content)
                        .map(|newline| newline + 1)
                        .filter(|start| *start < content_len),
                );
            }
        }
        self.readable.then_some(FileLines {
            content: &self.content,
            line_starts: &self.line_starts,
        })
    }
}

impl FileLines<'_> {
    fn line(&self, line_number: usize) -> Option<&[u8]> {
        let start = *self.line_starts.get(line_number.checked_sub(1)?)?;
        let end = memchr(b'\n', &self.content[start..])
            .map_or(self.content.len(), |offset| start + offset);
        Some(&self.content[start..end])
    }
}

fn write_match_with_context(
    out: &mut Vec<u8>,
    found: &Match,
    path: &[u8],
    lines: Option<&FileLines<'_>>,
    layout: &MatchLayout,
) {
    if let Some(lines) = lines {
        let first_line = found
            .line_number
            .saturating_sub(layout.context_lines)
            .max(1);
        write_context_range(
            out,
            path,
            lines,
            first_line..found.line_number,
            layout.max_line_len,
        );
    }
    out.extend_from_slice(b" - ");
    out.extend_from_slice(path);
    out.extend_from_slice(format!(":{}: ", found.line_number).as_bytes());
    write_clipped_line(out, found.line.as_bytes(), layout.max_line_len);
    if let Some(lines) = lines {
        write_context_range(
            out,
            path,
            lines,
            found.line_number + 1..found.line_number + layout.context_lines + 1,
            layout.max_line_len,
        );
    }
}

fn write_context_range(
    out: &mut Vec<u8>,
    path: &[u8],
    lines: &FileLines<'_>,
    line_numbers: Range<usize>,
    max_line_len: usize,
) {
    for line_number in line_numbers {
        let Some(line) = lines.line(line_number) else {
            break;
        };
        out.extend_from_slice(b"   ");
        out.extend_from_slice(path);
        out.extend_from_slice(format!(":{line_number}- ").as_bytes());
        write_clipped_line(out, line, max_line_len);
    }
}

fn write_clipped_line(out: &mut Vec<u8>, line: &[u8], max_line_len: usize) {
    out.extend_from_slice(&line[..line.len().min(max_line_len)]);
    if line.len() > max_line_len {
        out.extend_from_slice(b"...");
    }
    out.push(b'\n');
}

fn format_files_with_matches(
    workspace_root: &Path,
    pattern: &str,
    result: &GrepResult,
    offset: usize,
    limit: usize,
) -> String {
    let mut seen: HashSet<&PathBuf> = HashSet::new();
    let files: Vec<Vec<u8>> = result
        .matches
        .iter()
        .filter(|found| seen.insert(&found.absolute_path))
        .map(|found| display_path(workspace_root, &found.absolute_path))
        .collect();
    let page = Page::new(offset, limit, files.len());

    let mut out = Vec::new();
    if files.is_empty() {
        out.extend_from_slice(format!("[grep] no files with matches for {pattern}\n").as_bytes());
    } else if page.emitted() == 0 {
        out.extend_from_slice(
            format!(
                "[grep] no files with matches for {pattern} at offset {offset} ({} total files)\n",
                files.len()
            )
            .as_bytes(),
        );
    } else {
        out.extend_from_slice(
            format!(
                "[grep] {} files with matches for {pattern}{}\n",
                page.emitted(),
                page.header_suffix()
            )
            .as_bytes(),
        );
        for path in &files[page.start..page.end] {
            out.extend_from_slice(b" - ");
            out.extend_from_slice(path);
            out.push(b'\n');
        }
    }
    if page.end < files.len() {
        out.extend_from_slice(
            format!(
                "... more files available; use offset {} to continue\n",
                page.end
            )
            .as_bytes(),
        );
    }
    write_grep_result_notes(&mut out, result);
    sanitize_model_text_owned(out)
}

fn format_count(pattern: &str, count: &CountResult) -> String {
    let out = format!(
        "[grep] count {} matching lines in {} files for {pattern}\n{}",
        count.matching_lines,
        count.matching_files,
        render_candidate_notes(&count.candidates)
    );
    sanitize_model_text_owned(out.into_bytes())
}

fn write_grep_result_notes(out: &mut Vec<u8>, result: &GrepResult) {
    if result.truncated_reason == Some(TruncatedReason::CollectionCap) {
        out.extend_from_slice(
            format!("... match collection cap reached at {COLLECTION_CAP} matches before all candidate files were scanned\n")
                .as_bytes(),
        );
    }
    out.extend_from_slice(render_candidate_notes(&result.candidates).as_bytes());
}

fn display_path(workspace_root: &Path, absolute_path: &Path) -> Vec<u8> {
    if workspace_root.as_os_str().is_empty() {
        return absolute_path.as_os_str().as_bytes().to_vec();
    }
    workspace_relative_path(workspace_root, absolute_path)
        .into_os_string()
        .into_encoded_bytes()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use ofx_contract::{CallDescription, Concurrency, ToolEffect, ToolStatusDetail};
    use ofx_workspace::CandidateStats;
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::*;
    use crate::filesystem::DEFAULT_MAX_LIST_ENTRIES;
    use crate::filesystem::tests::{RememberedGrant, run_tool, run_tool_with};

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

        fn write(&self, relative: &str, content: impl AsRef<[u8]>) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            path
        }

        fn grep(&self, arguments: &Value) -> Result<String, ToolOutput> {
            GrepFilesArgs::decode(&arguments.to_string())?.execute(
                &FilesystemContext::new(&self.root),
                &PathAccess::WorkspaceOrExternal,
            )
        }
    }

    fn count_emitted_result_lines(body: &str) -> usize {
        body.lines().filter(|line| line.starts_with(" - ")).count()
    }

    fn matching_lines(count: usize) -> GrepResult {
        GrepResult {
            matches: (1..=count)
                .map(|line_number| Match {
                    absolute_path: PathBuf::from("many.txt"),
                    read_path: PathBuf::from("many.txt"),
                    line_number,
                    line: "needle".into(),
                })
                .collect(),
            truncated_reason: None,
            candidates: CandidateStats::default(),
        }
    }

    fn layout(head_limit: usize, max_list_entries: usize) -> MatchLayout {
        MatchLayout {
            head_limit,
            offset: 0,
            context_lines: 0,
            max_list_entries,
            max_line_len: 2000,
        }
    }

    #[test]
    fn grep_files_decodes_invalid_argument_shapes_as_failures() {
        let cases = [
            ("{", "grep_files arguments must be valid JSON"),
            ("[]", "grep_files arguments must be an object"),
            (
                "{\"path\":\".\"}",
                "grep_files requires string field \"pattern\"",
            ),
            (
                "{\"pattern\":1}",
                "grep_files field \"pattern\" must be a string",
            ),
            (
                "{\"pattern\":\"a\",\"head_limit\":0}",
                "grep_files field \"head_limit\" must be a positive integer",
            ),
            (
                "{\"pattern\":\"a\",\"offset\":-1}",
                "grep_files field \"offset\" must be a non-negative integer",
            ),
            (
                "{\"pattern\":\"a\",\"context_lines\":\"2\"}",
                "grep_files field \"context_lines\" must be a non-negative integer",
            ),
            (
                "{\"pattern\":\"a\",\"offset\":1e400}",
                "grep_files field \"offset\" must be a non-negative integer",
            ),
            (
                "{\"pattern\":\"a\",\"offset\":-0}",
                "grep_files field \"offset\" must be a non-negative integer",
            ),
            (
                "{\"pattern\":\"a\",\"mode\":\"count\",\"mode\":\"matches\"}",
                "grep_files arguments must be valid JSON",
            ),
        ];
        for (json, reason) in cases {
            assert_eq!(
                GrepFilesArgs::decode(json),
                Err(ToolOutput::failure(reason)),
                "{json}"
            );
        }
    }

    #[test]
    fn grep_files_validate_preserves_active_raw_pattern_and_path_values() {
        let decoded = GrepFilesArgs::decode("{\"pattern\":\" \\t\\n \",\"path\":\"   \"}").unwrap();
        assert_eq!(decoded.pattern, " \t\n ");
        assert_eq!(decoded.path, "   ");
    }

    #[test]
    fn grep_files_decodes_defaults_and_all_fields() {
        for json in [
            "{\"pattern\":\"needle\"}",
            "{\"pattern\":\"needle\",\"path\":\"\"}",
        ] {
            let decoded = GrepFilesArgs::decode(json).unwrap();
            assert_eq!(decoded.path, ".");
            assert_eq!(decoded.include, None);
            assert!(!decoded.case_insensitive);
            assert_eq!(decoded.head_limit, OUTPUT_CAP);
            assert_eq!(decoded.offset, 0);
        }
        let all = GrepFilesArgs::decode(
            "{\"pattern\":\"needle\",\"path\":\"src\",\"include\":\"*.zig\",\"case_insensitive\":true,\"head_limit\":5,\"offset\":2}",
        )
        .unwrap();
        assert_eq!(all.path, "src");
        assert_eq!(all.include.as_deref(), Some("*.zig"));
        assert!(all.case_insensitive);
        assert_eq!(all.head_limit, 5);
        assert_eq!(all.offset, 2);
        let lenient = GrepFilesArgs::decode(
            "{\"pattern\":\"needle\",\"path\":1,\"include\":1,\"case_insensitive\":\"yes\"}",
        )
        .unwrap();
        assert_eq!(lenient.path, ".");
        assert_eq!(lenient.include, None);
        assert!(!lenient.case_insensitive);
        assert_eq!(lenient.head_limit, OUTPUT_CAP);
    }

    #[test]
    fn grep_files_decodes_context_lines_above_cap_as_bounded() {
        let decoded =
            GrepFilesArgs::decode("{\"pattern\":\"needle\",\"context_lines\":99}").unwrap();
        assert_eq!(decoded.context_lines, CONTEXT_LINES_CAP);
        let limited = GrepFilesArgs::decode("{\"pattern\":\"needle\",\"head_limit\":999}").unwrap();
        assert_eq!(limited.head_limit, OUTPUT_CAP);
    }

    #[test]
    fn grep_files_reports_resolver_errors_as_model_visible_failures() {
        let workspace = Workspace::new();
        let failure = workspace
            .grep(&json!({"pattern": "needle", "path": "missing.txt"}))
            .unwrap_err();
        assert!(
            failure
                .content
                .starts_with("Unable to resolve grep search root: missing.txt (")
        );
    }

    #[test]
    fn grep_files_keeps_workspace_only_calls_inside_the_workspace() {
        let workspace = Workspace::new();
        let inside = workspace.root.join("workspace");
        fs::create_dir(&inside).unwrap();
        let external_file = workspace.write("external/outside.txt", "needle outside\n");
        let tool = GrepFiles::new(&inside);
        let external = workspace.root.join("external");
        let arguments = json!({ "pattern": "needle", "path": external }).to_string();

        let (_, held) = run_tool_with(&tool, &arguments, PathAccess::WorkspaceOnly);
        assert_eq!(
            held,
            ToolOutput::failure(format!(
                "Unable to resolve grep search root: {} (PathOutsideWorkspace)",
                external.display()
            ))
        );
        let (_, approved) = run_tool_with(&tool, &arguments, PathAccess::WorkspaceOrExternal);
        assert!(
            approved
                .content
                .contains(&format!("{}:1: needle outside", external_file.display())),
            "{}",
            approved.content
        );
    }

    #[test]
    fn grep_files_refuses_a_remembered_grant_root_swapped_out_of_its_tree() {
        let grant = RememberedGrant::new();
        let tool = GrepFiles::new(&grant.workspace);

        let (before, after) =
            grant.run_around_a_swap(&tool, r#"{"pattern":"needle","path":"../link"}"#);

        assert!(
            before.content.contains("/allowed/a.txt:1: needle allowed"),
            "{}",
            before.content
        );
        assert_eq!(
            after,
            ToolOutput::failure(
                "Unable to resolve grep search root: ../link (PathOutsideWorkspace)"
            )
        );
    }

    #[test]
    fn grep_files_reports_overlong_include_patterns_before_scanning() {
        let workspace = Workspace::new();
        let path = workspace.write("file.txt", "needle\n");
        let include = "a".repeat(MAX_PATTERN_BYTES + 1);

        assert_eq!(
            workspace.grep(&json!({"pattern": "needle", "path": path, "include": include})),
            Err(ToolOutput::failure(format!(
                "grep_files field \"include\" must be at most {MAX_PATTERN_BYTES} bytes"
            )))
        );
    }

    #[test]
    fn grep_files_single_file_non_text_root_returns_empty_matches() {
        let workspace = Workspace::new();
        let path = workspace.write("binary.txt", b"needle\x00binary\n");

        assert_eq!(
            workspace.grep(&json!({"pattern": "needle", "path": path})),
            Ok("[grep] no matches for needle\n".into())
        );
    }

    #[test]
    fn grep_files_single_file_oversized_root_returns_empty_matches() {
        let workspace = Workspace::new();
        let path = workspace.write("large.txt", vec![b'n'; 256 * 1024]);

        assert_eq!(
            workspace.grep(&json!({"pattern": "needle", "path": path})),
            Ok("[grep] no matches for needle\n".into())
        );
    }

    #[test]
    fn grep_files_reports_non_file_non_directory_roots() {
        let workspace = Workspace::new();
        let failure = workspace
            .grep(&json!({"pattern": "needle", "path": "/dev/null"}))
            .unwrap_err();
        assert_eq!(
            failure.content,
            "Not a regular file or directory: /dev/null"
        );
    }

    #[test]
    fn grep_files_single_file_search_returns_matching_lines() {
        let workspace = Workspace::new();
        let path = workspace.write("single.txt", "one\nneedle here\n");

        assert_eq!(
            workspace.grep(&json!({"pattern": "needle", "path": path})),
            Ok("[grep] 1 matches for needle\n - single.txt:2: needle here\n".into())
        );
    }

    #[test]
    fn grep_files_single_file_include_applies_to_basename() {
        let workspace = Workspace::new();
        let path = workspace.write("src/single.zig", "needle\n");

        assert_eq!(
            workspace.grep(&json!({"pattern": "needle", "path": path, "include": "*.zig"})),
            Ok("[grep] 1 matches for needle\n - src/single.zig:1: needle\n".into())
        );
        assert_eq!(
            workspace.grep(&json!({"pattern": "needle", "path": path, "include": "src/*.zig"})),
            Ok("[grep] no matches for needle\n".into())
        );
    }

    #[test]
    fn grep_files_walks_multiple_files_without_sorting() {
        let workspace = Workspace::new();
        workspace.write("src/a.txt", "needle a\n");
        workspace.write("src/nested/b.txt", "needle b\n");
        workspace.write("src/c.txt", "other\n");

        let result = workspace.grep(&json!({"pattern": "needle"})).unwrap();

        assert!(result.starts_with("[grep] 2 matches for needle\n"));
        assert!(result.contains("src/a.txt:1: needle a"));
        assert!(result.contains("src/nested/b.txt:1: needle b"));
        assert!(!result.contains("src/c.txt"));
    }

    #[test]
    fn grep_files_finds_match_beyond_former_traversal_cap() {
        let workspace = Workspace::new();
        for index in 0..2050 {
            workspace.write(&format!("many/file-{index:04}.txt"), "not here\n");
        }
        workspace.write("zzzz/match.txt", "needle late\n");

        let result = workspace.grep(&json!({"pattern": "needle"})).unwrap();

        assert!(result.contains("zzzz/match.txt:1: needle late"));
        assert!(!result.contains("traversal cap"));
    }

    #[test]
    fn grep_files_include_filters_walked_files() {
        let workspace = Workspace::new();
        workspace.write("src/main.zig", "needle in zig\n");
        workspace.write("src/notes.txt", "needle in text\n");

        let result = workspace
            .grep(&json!({"pattern": "needle", "include": "*.zig"}))
            .unwrap();

        assert!(result.contains("src/main.zig:1: needle in zig"));
        assert!(!result.contains("notes.txt"));
    }

    #[test]
    fn grep_files_supports_ascii_case_insensitive_matching() {
        let workspace = Workspace::new();
        workspace.write("case.txt", "Needle Here\n");

        assert_eq!(
            workspace.grep(&json!({"pattern": "needle", "case_insensitive": true})),
            Ok("[grep] 1 matches for needle\n - case.txt:1: Needle Here\n".into())
        );
    }

    #[test]
    fn grep_files_paginates_matching_line_output() {
        let workspace = Workspace::new();
        let path = workspace.write("matches.txt", "needle one\nneedle two\nneedle three\n");

        assert_eq!(
            workspace
                .grep(&json!({"pattern": "needle", "path": path, "head_limit": 1, "offset": 1})),
            Ok(concat!(
                "[grep] 1 matches for needle (showing 2-2 of 3)\n",
                " - matches.txt:2: needle two\n",
                "... more matches available; use offset 2 to continue\n"
            )
            .into())
        );
    }

    #[test]
    fn grep_files_context_lines_includes_nearby_lines_around_emitted_matches() {
        let workspace = Workspace::new();
        workspace.write("context.txt", "one\ntwo\nneedle here\nfour\nfive\n");

        assert_eq!(
            workspace
                .grep(&json!({"pattern": "needle", "path": "context.txt", "context_lines": 1})),
            Ok(concat!(
                "[grep] 1 matches for needle\n",
                "   context.txt:2- two\n",
                " - context.txt:3: needle here\n",
                "   context.txt:4- four\n"
            )
            .into())
        );
    }

    #[test]
    fn grep_files_context_lines_skips_synthetic_trailing_line_after_final_newline() {
        let workspace = Workspace::new();
        workspace.write("trailing.txt", "before\nneedle\n");

        assert_eq!(
            workspace
                .grep(&json!({"pattern": "needle", "path": "trailing.txt", "context_lines": 1})),
            Ok(concat!(
                "[grep] 1 matches for needle\n",
                "   trailing.txt:1- before\n",
                " - trailing.txt:2: needle\n"
            )
            .into())
        );
    }

    #[test]
    fn grep_files_context_lines_skips_after_context_at_eof_without_final_newline() {
        let workspace = Workspace::new();
        workspace.write("no-trailing.txt", "before\nneedle");

        assert_eq!(
            workspace
                .grep(&json!({"pattern": "needle", "path": "no-trailing.txt", "context_lines": 1})),
            Ok(concat!(
                "[grep] 1 matches for needle\n",
                "   no-trailing.txt:1- before\n",
                " - no-trailing.txt:2: needle\n"
            )
            .into())
        );
    }

    #[test]
    fn grep_files_context_lines_preserves_real_blank_lines_in_range() {
        let workspace = Workspace::new();
        workspace.write("blank-context.txt", "before\nneedle\n\nafter\n");

        assert_eq!(
            workspace.grep(
                &json!({"pattern": "needle", "path": "blank-context.txt", "context_lines": 1})
            ),
            Ok(concat!(
                "[grep] 1 matches for needle\n",
                "   blank-context.txt:1- before\n",
                " - blank-context.txt:2: needle\n",
                "   blank-context.txt:3- \n"
            )
            .into())
        );
    }

    #[test]
    fn grep_files_context_lines_follows_match_pagination() {
        let workspace = Workspace::new();
        workspace.write(
            "paged.txt",
            "before one\nneedle one\nafter one\nbefore two\nneedle two\nafter two\n",
        );

        assert_eq!(
            workspace.grep(&json!({"pattern": "needle", "path": "paged.txt", "head_limit": 1, "offset": 1, "context_lines": 1})),
            Ok(concat!(
                "[grep] 1 matches for needle (showing 2-2 of 2)\n",
                "   paged.txt:4- before two\n",
                " - paged.txt:5: needle two\n",
                "   paged.txt:6- after two\n"
            )
            .into())
        );
    }

    #[test]
    fn grep_files_context_lines_reload_when_consecutive_matches_change_files() {
        let workspace = Workspace::new();
        workspace.write("a.txt", "a before\nneedle a\na after\nneedle again\n");
        workspace.write("b.txt", "b before\nneedle b\n");

        let result = workspace
            .grep(&json!({"pattern": "needle", "context_lines": 1}))
            .unwrap();

        assert!(result.starts_with("[grep] 3 matches for needle\n"));
        assert!(result.contains(concat!(
            "   a.txt:1- a before\n",
            " - a.txt:2: needle a\n",
            "   a.txt:3- a after\n",
            "   a.txt:3- a after\n",
            " - a.txt:4: needle again\n"
        )));
        assert!(result.contains("   b.txt:1- b before\n - b.txt:2: needle b\n"));
    }

    #[test]
    fn grep_files_context_lines_read_a_symlinked_match_through_its_resolved_target() {
        let workspace = Workspace::new();
        workspace.write("target/internal.txt", "before\nneedle\nafter\n");
        symlink("target/internal.txt", workspace.root.join("link.txt")).unwrap();

        assert_eq!(
            workspace
                .grep(&json!({"pattern": "needle", "include": "link.txt", "context_lines": 1})),
            Ok(concat!(
                "[grep] 1 matches for needle\n",
                "   link.txt:1- before\n",
                " - link.txt:2: needle\n",
                "   link.txt:3- after\n"
            )
            .into())
        );
    }

    #[test]
    fn grep_files_context_lines_does_not_affect_files_with_matches_or_count_modes() {
        let workspace = Workspace::new();
        workspace.write("modes.txt", "before\nneedle\nafter\n");

        assert_eq!(
            workspace.grep(&json!({"pattern": "needle", "path": "modes.txt", "mode": "files_with_matches", "context_lines": 1})),
            Ok("[grep] 1 files with matches for needle\n - modes.txt\n".into())
        );
        assert_eq!(
            workspace.grep(&json!({"pattern": "needle", "path": "modes.txt", "mode": "count", "context_lines": 1})),
            Ok("[grep] count 1 matching lines in 1 files for needle\n".into())
        );
    }

    #[test]
    fn grep_files_files_with_matches_mode_returns_unique_paginated_paths() {
        let workspace = Workspace::new();
        workspace.write("one.txt", "needle first\nneedle second\n");
        workspace.write("two.txt", "needle third\n");

        let result = workspace
            .grep(&json!({"pattern": "needle", "mode": "files_with_matches", "head_limit": 10}))
            .unwrap();

        assert!(result.starts_with("[grep] 2 files with matches for needle\n"));
        assert!(result.contains(" - one.txt\n"));
        assert!(result.contains(" - two.txt\n"));
        assert_eq!(count_emitted_result_lines(&result), 2);
    }

    #[test]
    fn grep_files_formatter_uses_active_pagination_cap() {
        let body = format_matches(Path::new(""), "needle", &matching_lines(3), &layout(2, 2));

        assert_eq!(
            body,
            concat!(
                "[grep] 2 matches for needle (showing 1-2 of 3)\n",
                " - many.txt:1: needle\n",
                " - many.txt:2: needle\n",
                "... more matches available; use offset 2 to continue\n"
            )
        );
    }

    #[test]
    fn grep_files_formatter_does_not_report_output_truncation_for_exactly_active_cap() {
        let body = format_matches(
            Path::new(""),
            "needle",
            &matching_lines(DEFAULT_MAX_LIST_ENTRIES),
            &layout(OUTPUT_CAP, DEFAULT_MAX_LIST_ENTRIES),
        );

        assert!(body.starts_with("[grep] 100 matches for needle\n"));
        assert_eq!(count_emitted_result_lines(&body), DEFAULT_MAX_LIST_ENTRIES);
        assert!(!body.contains("more matches available"));
    }

    #[test]
    fn grep_files_formatter_reports_active_truncation_when_cap_hides_matches() {
        let body = format_matches(
            Path::new(""),
            "needle",
            &matching_lines(DEFAULT_MAX_LIST_ENTRIES + 1),
            &layout(OUTPUT_CAP, DEFAULT_MAX_LIST_ENTRIES),
        );

        assert!(body.starts_with("[grep] 100 matches for needle (showing 1-100 of 101)\n"));
        assert_eq!(count_emitted_result_lines(&body), DEFAULT_MAX_LIST_ENTRIES);
        assert!(body.ends_with("... more matches available; use offset 100 to continue\n"));
    }

    #[test]
    fn grep_files_formatter_reports_collection_cap_separately_from_output_truncation() {
        let result = GrepResult {
            truncated_reason: Some(TruncatedReason::CollectionCap),
            ..matching_lines(COLLECTION_CAP)
        };
        let body = format_matches(
            Path::new(""),
            "needle",
            &result,
            &layout(OUTPUT_CAP, DEFAULT_MAX_LIST_ENTRIES),
        );

        assert!(body.starts_with("[grep] 100 matches for needle (showing 1-100 of 2000)\n"));
        assert_eq!(count_emitted_result_lines(&body), DEFAULT_MAX_LIST_ENTRIES);
        assert!(body.contains("... more matches available; use offset 100 to continue\n"));
        assert!(body.contains("match collection cap reached at 2000 matches"));
    }

    #[test]
    fn grep_files_count_mode_reports_exact_matching_lines_beyond_collection_cap() {
        let workspace = Workspace::new();
        workspace.write("many.txt", "needle\n".repeat(COLLECTION_CAP + 1));

        assert_eq!(
            workspace.grep(&json!({"pattern": "needle", "mode": "count"})),
            Ok("[grep] count 2001 matching lines in 1 files for needle\n".into())
        );
    }

    #[test]
    fn grep_files_formatter_reports_candidate_cap_without_output_truncation_wording() {
        let result = GrepResult {
            matches: Vec::new(),
            truncated_reason: None,
            candidates: CandidateStats {
                incomplete: true,
                ..CandidateStats::default()
            },
        };
        let body = format_matches(
            Path::new(""),
            "needle",
            &result,
            &layout(OUTPUT_CAP, DEFAULT_MAX_LIST_ENTRIES),
        );

        assert_eq!(
            body,
            "[grep] no matches for needle\n... candidate list may be incomplete; candidate cap 100000 reached before all files were discovered\n"
        );
    }

    #[test]
    fn grep_files_describes_read_only_calls_and_admits_the_search_root() {
        let workspace = Workspace::new();
        workspace.write("a.txt", "needle\n");
        let tool = GrepFiles::new(&workspace.root);

        let (description, output) = run_tool(&tool, "{\"pattern\":\"needle\"}");
        assert_eq!(
            description,
            CallDescription {
                title: "Searching needle".to_owned(),
                label: Some(PRESENTATION.label("needle")),
                activity: ToolActivity::Read,
                effect: ToolEffect::ReadOnly,
                concurrency: Concurrency::Parallel,
            }
        );
        assert_eq!(
            output,
            ToolOutput::success("[grep] 1 matches for needle\n - a.txt:1: needle\n")
        );

        let (description, output) = run_tool(&tool, "{\"pattern\":\"needle\",\"path\":\"nope\"}");
        assert_eq!(description.effect, ToolEffect::ReadOnly);
        assert_eq!(
            output,
            ToolOutput::failure("Path not found: nope")
                .with_status_detail(ToolStatusDetail::PreflightFailed)
        );

        let (description, output) = run_tool(&tool, "{\"pattern\":\"needle\",\"offset\":-1}");
        assert_eq!(description.title, "Searching needle");
        assert_eq!(description.effect, ToolEffect::None);
        assert_eq!(
            output,
            ToolOutput::failure("grep_files field \"offset\" must be a non-negative integer")
        );
    }
}
