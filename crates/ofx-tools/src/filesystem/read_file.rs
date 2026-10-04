use std::io::{Read, Write};
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use memchr::{memchr, memchr_iter};
use ofx_contract::{
    CallDescription, CallPresentation, ExecutionFailure, PathAccess, PreparedCall, Tool,
    ToolActivity, ToolEffect, ToolOutput, ToolSpec, filesystem_access_denied_json,
    plain_description, tool_execution_failure_json,
};
use ofx_text::{is_model_safe_text, sanitize_model_text_owned};
use ofx_workspace::{
    PATH_ENTRY_WHITESPACE, PathError, RegularFileError, open_regular_file, path_inside,
    resolve_workspace_or_external_path, workspace_relative_path,
};

use super::{DEFAULT_MAX_READ_FILE_LINES, FilesystemContext, read_only_effect, tool_spec};
use crate::tool_admission::admit_existing_path;
use crate::tool_args::{optional_integer, parse_arguments, required_string};
use crate::tool_runtime::BlockingCall;

const TOOL_NAME: &str = "read_file";
const DESCRIPTION: &str = "Read one file with bounded line-numbered output and optional start_line/line_count range. UTF-8 text returns as numbered lines; image files (PNG, JPEG, GIF, WebP up to 3.9MB) attach to the result so you can see them. Paths may be workspace-relative or external using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy. When to use: inspect an exact known path before editing or explaining code, or view an image file. When NOT to use: list directories, search many files, read non-image binary data, or bypass dedicated search tools.";
const INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"File path relative to the workspace root, or an external path using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy."},"start_line":{"type":"integer","description":"Optional 1-based first line to return. Defaults to 1."},"line_count":{"type":"integer","description":"Optional positive number of lines to return. Defaults to the normal read cap and is bounded."}},"required":["path"]}"#;
const PRESENTATION: CallPresentation = CallPresentation {
    activity: ToolActivity::Read,
    action_label: "Reading",
    completed_label: "Read",
    label_argument: "path",
    label_default: "file",
};
const MAX_SNAPSHOT_FILE_BYTES: usize = 10 * 1024 * 1024;
const MAX_MODEL_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_LINE_COUNT: usize = 2000;
const LINE_TRUNCATED_SUFFIX: &[u8] = b"... (line truncated)";

pub struct ReadFile {
    spec: ToolSpec,
    context: Arc<FilesystemContext>,
}

impl ReadFile {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self::with_context(FilesystemContext::new(workspace_root))
    }

    fn with_context(context: FilesystemContext) -> Self {
        Self {
            spec: tool_spec(TOOL_NAME, DESCRIPTION, INPUT_SCHEMA),
            context: Arc::new(context),
        }
    }
}

impl Tool for ReadFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn provisional_presentation(&self) -> Option<CallPresentation> {
        Some(PRESENTATION)
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        let decoded = ReadFileArgs::decode(arguments);
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

    fn describe_saved(&self, arguments: &str) -> Option<CallDescription> {
        Some(plain_description(
            TOOL_NAME,
            &PRESENTATION,
            arguments,
            ToolEffect::None,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadFileArgs {
    requested_path: String,
    path: String,
    start_line: usize,
    line_count: usize,
}

impl ReadFileArgs {
    fn decode(args_json: &str) -> Result<Self, ToolOutput> {
        let arguments = parse_arguments(TOOL_NAME, args_json)?;
        let requested_path = required_string(TOOL_NAME, &arguments, "path")?;
        let start_line = optional_integer(TOOL_NAME, &arguments, "start_line", 1, "positive")?;
        let line_count = optional_integer(TOOL_NAME, &arguments, "line_count", 1, "positive")?;
        let path = requested_path
            .trim_matches(PATH_ENTRY_WHITESPACE)
            .to_owned();
        if path.is_empty() {
            return Err(ToolOutput::failure(
                "read_file field \"path\" must not be empty",
            ));
        }
        Ok(Self {
            requested_path,
            path,
            start_line: start_line.unwrap_or(1),
            line_count: line_count.map_or(DEFAULT_MAX_READ_FILE_LINES, |count| {
                count.min(MAX_LINE_COUNT)
            }),
        })
    }

    fn run(&self, context: &FilesystemContext, path_access: &PathAccess) -> ToolOutput {
        if let Err(failure) =
            admit_existing_path(TOOL_NAME, &context.workspace_root, &self.requested_path)
        {
            return failure;
        }
        match self.read(context, path_access) {
            Ok((text, covered)) => ToolOutput::success(text).covering_full_file(covered),
            Err(failure) => failure,
        }
    }

    fn read(
        &self,
        context: &FilesystemContext,
        path_access: &PathAccess,
    ) -> Result<(String, bool), ToolOutput> {
        let target = self.resolve(context, path_access)?;
        self.read_target(context, &target)
    }

    fn resolve(
        &self,
        context: &FilesystemContext,
        path_access: &PathAccess,
    ) -> Result<PathBuf, ToolOutput> {
        let failure = |error| read_file_failure(RegularFileError::Path(error), &self.path);
        let target = resolve_workspace_or_external_path(&context.workspace_root, &self.path)
            .map_err(failure)?;
        if path_access
            .confining_root(&context.workspace_root)
            .is_some_and(|root| !path_inside(root, &target))
        {
            return Err(failure(PathError::PathOutsideWorkspace));
        }
        Ok(target)
    }

    fn read_target(
        &self,
        context: &FilesystemContext,
        target: &Path,
    ) -> Result<(String, bool), ToolOutput> {
        let target_text = target.to_string_lossy();
        let (file, metadata) = open_regular_file(target)
            .map_err(|failure| read_file_failure(failure, &target_text))?;

        let size = metadata.len();
        let truncated_by_size = size > MAX_SNAPSHOT_FILE_BYTES as u64;
        let mut snapshot = Vec::new();
        file.take(size.min(MAX_SNAPSHOT_FILE_BYTES as u64))
            .read_to_end(&mut snapshot)
            .map_err(|error| {
                read_file_failure(RegularFileError::Path(error.into()), &target_text)
            })?;
        let relative = workspace_relative_path(&context.workspace_root, target);
        let display_path = relative.as_os_str().as_bytes();
        let snapshot_covers_full_file = !truncated_by_size && snapshot.len() as u64 == size;

        if !is_model_safe_text(&snapshot) {
            let mut text = Vec::new();
            text.extend_from_slice(b"<path>");
            text.extend_from_slice(display_path);
            let _ = write!(
                text,
                "</path>\n<content>binary or non-utf8 file omitted ({size} bytes)</content>"
            );
            return Ok((sanitize_model_text_owned(text), false));
        }

        let line_count = self.line_count.min(context.max_read_file_lines);
        let scan = select_lines(
            &snapshot,
            self.start_line,
            line_count,
            context.max_read_file_line_len,
        );
        let covered = snapshot_covers_full_file && scan.covers_full_file(self.start_line);
        let text = format_read_output(
            display_path,
            self.start_line,
            &snapshot,
            &scan,
            snapshot_covers_full_file,
        );
        Ok((sanitize_model_text_owned(text), covered))
    }
}

fn read_file_failure(failure: RegularFileError, path: &str) -> ToolOutput {
    let body = match failure {
        RegularFileError::Path(error) if error.is_access_denied() => {
            filesystem_access_denied_json(TOOL_NAME, path, &error.to_string())
        }
        RegularFileError::NotRegularFile => tool_execution_failure_json(&ExecutionFailure {
            tool_name: TOOL_NAME,
            message: "read_file requires a regular file",
            details: &[
                ("field", "path"),
                ("path", path),
                ("error", "NotRegularFile"),
            ],
            suggestion: Some(
                "Use glob_files to inspect directory contents, then choose a regular file.",
            ),
        }),
        RegularFileError::Path(error) => tool_execution_failure_json(&ExecutionFailure {
            tool_name: TOOL_NAME,
            message: "read_file failed",
            details: &[
                ("field", "path"),
                ("path", path),
                ("error", &error.to_string()),
            ],
            suggestion: Some(
                "Run glob_files to discover matching paths, or check the path relative to the workspace.",
            ),
        }),
    };
    ToolOutput::failure(body)
}

struct LineRecord {
    number: usize,
    range: Range<usize>,
    truncated: bool,
}

struct LineSelection {
    start_line: usize,
    line_count: usize,
    max_line_len: usize,
    records: Vec<LineRecord>,
    display_truncated: bool,
    budget_width: usize,
    budget_bytes: usize,
}

struct ReadScan {
    total_lines: usize,
    display_truncated: bool,
    records: Vec<LineRecord>,
}

impl ReadScan {
    fn covers_full_file(&self, start_line: usize) -> bool {
        !self.display_truncated && start_line == 1 && self.records.len() == self.total_lines
    }
}

impl LineSelection {
    fn new(start_line: usize, line_count: usize, max_line_len: usize) -> Self {
        Self {
            start_line,
            line_count,
            max_line_len,
            records: Vec::new(),
            display_truncated: false,
            budget_width: 1,
            budget_bytes: 0,
        }
    }

    fn keep_line(&mut self, line_number: usize, start: usize, line: &[u8]) -> bool {
        if line_number < self.start_line {
            return true;
        }
        if self.records.len() >= self.line_count {
            self.display_truncated = true;
            return false;
        }
        let width = digit_count(line_number);
        if width > self.budget_width {
            self.budget_bytes += self.records.len() * (width - self.budget_width);
            self.budget_width = width;
        }
        let truncated = line.len() > self.max_line_len;
        let clipped_len = clip_at_char_boundary(line, self.max_line_len);
        let display_len = clipped_len
            + if truncated {
                LINE_TRUNCATED_SUFFIX.len()
            } else {
                0
            };
        let rendered_bytes =
            self.budget_bytes + rendered_line_bytes(self.budget_width, display_len);
        if rendered_bytes > MAX_MODEL_OUTPUT_BYTES {
            self.display_truncated = true;
            return false;
        }
        self.display_truncated |= truncated;
        self.records.push(LineRecord {
            number: line_number,
            range: start..start + clipped_len,
            truncated,
        });
        self.budget_bytes = rendered_bytes;
        true
    }
}

fn count_lines(content: &[u8]) -> usize {
    if content.is_empty() {
        return 0;
    }
    memchr_iter(b'\n', content).count() + usize::from(content.last() != Some(&b'\n'))
}

fn select_lines(
    content: &[u8],
    start_line: usize,
    line_count: usize,
    max_line_len: usize,
) -> ReadScan {
    let mut selection = LineSelection::new(start_line, line_count, max_line_len);
    let mut line_number = 1;
    let mut start = 0;
    while start < content.len() {
        let end = memchr(b'\n', &content[start..]).map_or(content.len(), |offset| start + offset);
        if !selection.keep_line(line_number, start, &content[start..end]) {
            break;
        }
        start = end + 1;
        line_number += 1;
    }
    ReadScan {
        total_lines: count_lines(content),
        display_truncated: selection.display_truncated,
        records: selection.records,
    }
}

fn format_read_output(
    display_path: &[u8],
    start_line: usize,
    content: &[u8],
    scan: &ReadScan,
    snapshot_covers_full_file: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"<path>");
    out.extend_from_slice(display_path);
    out.extend_from_slice(b"</path>\n<content>\n");
    if let Some(last) = scan.records.last() {
        let width = digit_count(last.number);
        for record in &scan.records {
            let _ = write!(out, "{:<width$}\t", record.number);
            out.extend_from_slice(&content[record.range.clone()]);
            if record.truncated {
                out.extend_from_slice(LINE_TRUNCATED_SUFFIX);
            }
            out.push(b'\n');
        }
    } else if scan.total_lines > 0 && start_line > scan.total_lines {
        let _ = writeln!(
            out,
            "... [start_line {start_line} is beyond end of file; total lines {}]",
            scan.total_lines
        );
    }

    let include_sentinel = !scan.covers_full_file(start_line) || !snapshot_covers_full_file;
    if include_sentinel && (!scan.records.is_empty() || scan.display_truncated) {
        let shown = scan.records.len();
        let total = scan.total_lines;
        let _ = if snapshot_covers_full_file {
            writeln!(
                out,
                "... [showing {shown} of {total} lines; use start_line/line_count to read more.]"
            )
        } else {
            writeln!(
                out,
                "... [showing {shown} of at least {total} lines; file snapshot was capped before EOF.]"
            )
        };
    }
    out.extend_from_slice(b"</content>");
    out
}

fn clip_at_char_boundary(line: &[u8], max_len: usize) -> usize {
    let mut end = max_len.min(line.len());
    while line.get(end).is_some_and(|byte| byte & 0xC0 == 0x80) {
        end -= 1;
    }
    end
}

fn rendered_line_bytes(width: usize, text_len: usize) -> usize {
    width + 1 + text_len + 1
}

fn digit_count(value: usize) -> usize {
    value
        .checked_ilog10()
        .map_or(1, |digits| digits as usize + 1)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::os::unix::fs::symlink;

    use ofx_contract::{
        CallDescription, Concurrency, ToolEffect, ToolResultStatus, ToolStatusDetail,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::filesystem::DEFAULT_MAX_READ_FILE_LINE_LEN;
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

        fn context(&self) -> FilesystemContext {
            FilesystemContext::new(&self.root)
        }
    }

    fn rootless_context() -> FilesystemContext {
        FilesystemContext::new("")
    }

    fn path_args(path: &Path) -> String {
        serde_json::json!({ "path": path }).to_string()
    }

    fn read(context: &FilesystemContext, args_json: &str) -> Result<String, ToolOutput> {
        ReadFileArgs::decode(args_json)?
            .read(context, &PathAccess::WorkspaceOrExternal)
            .map(|(text, _)| text)
    }

    fn text(context: &FilesystemContext, args_json: &str) -> String {
        read(context, args_json).unwrap()
    }

    fn long_line_text(line_count: usize) -> String {
        "x\n".repeat(line_count)
    }

    #[test]
    fn read_file_decodes_invalid_argument_shapes_as_failures() {
        let cases = [
            ("{", "read_file arguments must be valid JSON"),
            ("[]", "read_file arguments must be an object"),
            ("{}", "read_file requires string field \"path\""),
            ("{\"path\":1}", "read_file field \"path\" must be a string"),
            (
                "{\"path\":\"a\",\"start_line\":0}",
                "read_file field \"start_line\" must be a positive integer",
            ),
            (
                "{\"path\":\"a\",\"line_count\":1.5}",
                "read_file field \"line_count\" must be a positive integer",
            ),
            (
                "{\"path\":\" \\n\"}",
                "read_file field \"path\" must not be empty",
            ),
        ];
        for (json, reason) in cases {
            assert_eq!(
                ReadFileArgs::decode(json),
                Err(ToolOutput::failure(reason)),
                "{json}"
            );
        }
        let decoded = ReadFileArgs::decode("{\"path\":\"a\",\"line_count\":99999}").unwrap();
        assert_eq!(decoded.line_count, MAX_LINE_COUNT);
        let defaults = ReadFileArgs::decode("{\"path\":\"a\"}").unwrap();
        assert_eq!(defaults.line_count, DEFAULT_MAX_READ_FILE_LINES);
    }

    #[test]
    fn read_file_decodes_arguments_with_upstream_json_value_rules() {
        let rejected = [
            (
                r#"{"path":"a","line_count":1.0}"#,
                "read_file field \"line_count\" must be a positive integer",
            ),
            (
                r#"{"path":"a","line_count":-0}"#,
                "read_file field \"line_count\" must be a positive integer",
            ),
            (
                r#"{"path":"a","start_line":9223372036854775808}"#,
                "read_file field \"start_line\" must be a positive integer",
            ),
            (
                r#"{"path":"a","line_count":1e400}"#,
                "read_file field \"line_count\" must be a positive integer",
            ),
            (
                r#"{"path":"a","line_count":null}"#,
                "read_file field \"line_count\" must be a positive integer",
            ),
            (
                r#"{"path":"a","path":"b"}"#,
                "read_file arguments must be valid JSON",
            ),
            (
                r#"{"path":"a","extra":{"k":1,"k":2}}"#,
                "read_file arguments must be valid JSON",
            ),
        ];
        for (json, reason) in rejected {
            assert_eq!(
                ReadFileArgs::decode(json),
                Err(ToolOutput::failure(reason)),
                "{json}"
            );
        }

        let largest = i64::MAX;
        let decoded = ReadFileArgs::decode(&format!(
            r#"{{"path":"a","start_line":{largest},"line_count":{largest}}}"#
        ))
        .unwrap();
        assert_eq!(decoded.start_line, 9_223_372_036_854_775_807);
        assert_eq!(decoded.line_count, MAX_LINE_COUNT);
        let nested = format!(
            r#"{{"path":"a","extra":{}{}}}"#,
            "[".repeat(512),
            "]".repeat(512)
        );
        assert_eq!(ReadFileArgs::decode(&nested).unwrap().path, "a");
    }

    #[test]
    fn read_file_describes_calls_with_invalid_arguments_as_having_no_effect() {
        let workspace = Workspace::new();
        let tool = ReadFile::with_context(workspace.context());

        let (description, output) = run_tool(&tool, r#"{"path":"a.txt","start_line":0}"#);

        assert_eq!(description.title, "Reading a.txt");
        assert_eq!(description.effect, ToolEffect::None);
        assert_eq!(description.concurrency, Concurrency::Parallel);
        assert_eq!(
            output,
            ToolOutput::failure("read_file field \"start_line\" must be a positive integer")
        );
    }

    #[test]
    fn read_file_reads_workspace_relative_path() {
        let workspace = Workspace::new();
        workspace.write("notes/today.txt", "hello\n");

        assert_eq!(
            text(&workspace.context(), "{\"path\":\"notes/today.txt\"}"),
            "<path>notes/today.txt</path>\n<content>\n1\thello\n</content>"
        );
    }

    #[test]
    fn read_file_external_absolute_path_preserves_absolute_display() {
        let workspace = Workspace::new();
        fs::create_dir(workspace.root.join("workspace")).unwrap();
        let external = workspace.write("external.txt", "outside\n");
        let context = FilesystemContext::new(workspace.root.join("workspace"));

        assert_eq!(
            text(&context, &path_args(&external)),
            format!(
                "<path>{}</path>\n<content>\n1\toutside\n</content>",
                external.display()
            )
        );
    }

    #[test]
    fn read_file_keeps_workspace_only_calls_inside_the_workspace() {
        let workspace = Workspace::new();
        let inside = workspace.write("workspace/dir/notes.txt", "inside\n");
        let secret = workspace.write("outside/notes.txt", "outside secret\n");
        let tool = ReadFile::with_context(FilesystemContext::new(workspace.root.join("workspace")));
        let held = |path: &Path| {
            format!(
                "{{\"error\":{{\"type\":\"tool_execution_failed\",\"tool_name\":\"read_file\",\"message\":\"read_file failed\",\"details\":{{\"field\":\"path\",\"path\":\"{}\",\"error\":\"PathOutsideWorkspace\"}},\"suggestion\":\"Run glob_files to discover matching paths, or check the path relative to the workspace.\"}}}}",
                path.display()
            )
        };

        let (_, external) = run_tool_with(&tool, &path_args(&secret), PathAccess::WorkspaceOnly);
        assert_eq!(external, ToolOutput::failure(held(&secret)));
        let (_, approved) =
            run_tool_with(&tool, &path_args(&secret), PathAccess::WorkspaceOrExternal);
        assert_eq!(
            approved,
            ToolOutput::success(format!(
                "<path>{}</path>\n<content>\n1\toutside secret\n</content>",
                secret.display()
            ))
            .covering_full_file(true)
        );

        fs::rename(
            workspace.root.join("workspace/dir"),
            workspace.root.join("workspace/moved"),
        )
        .unwrap();
        symlink(
            workspace.root.join("outside"),
            workspace.root.join("workspace/dir"),
        )
        .unwrap();
        let (_, swapped) = run_tool_with(&tool, &path_args(&inside), PathAccess::WorkspaceOnly);
        assert_eq!(swapped, ToolOutput::failure(held(&inside)));
    }

    #[test]
    fn read_file_refuses_a_remembered_grant_path_swapped_out_of_its_tree() {
        let grant = RememberedGrant::new();
        let tool = ReadFile::new(&grant.workspace);

        let (before, after) = grant.run_around_a_swap(&tool, r#"{"path":"../link/a.txt"}"#);

        assert!(
            before
                .content
                .ends_with("<content>\n1\tneedle allowed\n</content>"),
            "{}",
            before.content
        );
        assert_eq!(
            after,
            ToolOutput::failure(
                r#"{"error":{"type":"tool_execution_failed","tool_name":"read_file","message":"read_file failed","details":{"field":"path","path":"../link/a.txt","error":"PathOutsideWorkspace"},"suggestion":"Run glob_files to discover matching paths, or check the path relative to the workspace."}}"#
            )
        );
    }

    #[test]
    fn read_file_access_denial_returns_structured_recovery() {
        let failure = read_file_failure(
            RegularFileError::Path(PathError::AccessDenied),
            "/tmp/blocked/read.txt",
        );
        assert_eq!(failure.status, ToolResultStatus::Failure);
        let body = failure.content;
        assert!(body.starts_with("{\"error\":{\"type\":\"tool_execution_failed\""));
        assert!(body.contains("\"tool_name\":\"read_file\""));
        assert!(body.contains("/tmp/blocked/read.txt"));
        assert!(body.contains("AccessDenied"));
        assert!(body.contains("symlink"));
    }

    #[test]
    fn read_file_non_regular_paths_return_structured_recovery() {
        let body = read_file_failure(RegularFileError::NotRegularFile, "/tmp/search-pipe").content;
        assert!(body.starts_with("{\"error\":{\"type\":\"tool_execution_failed\""));
        assert!(body.contains("read_file requires a regular file"));
        assert!(body.contains("NotRegularFile"));
        assert!(body.contains("glob_files"));
    }

    #[test]
    fn read_file_directories_return_structured_recovery() {
        let workspace = Workspace::new();
        fs::create_dir(workspace.root.join("dir")).unwrap();

        let failure = read(&workspace.context(), "{\"path\":\"dir\"}").unwrap_err();

        assert_eq!(
            failure.content,
            format!(
                "{{\"error\":{{\"type\":\"tool_execution_failed\",\"tool_name\":\"read_file\",\"message\":\"read_file requires a regular file\",\"details\":{{\"field\":\"path\",\"path\":\"{}\",\"error\":\"NotRegularFile\"}},\"suggestion\":\"Use glob_files to inspect directory contents, then choose a regular file.\"}}}}",
                workspace.root.join("dir").display()
            )
        );
    }

    #[test]
    fn read_file_never_returns_outside_content_when_a_parent_is_swapped_after_resolution() {
        let workspace = Workspace::new();
        workspace.write("dir/notes.txt", "inside\n");
        let outside = Workspace::new();
        outside.write("notes.txt", "outside secret\n");
        let context = workspace.context();
        let arguments = ReadFileArgs::decode(r#"{"path":"dir/notes.txt"}"#).unwrap();

        let target = arguments
            .resolve(&context, &PathAccess::WorkspaceOnly)
            .unwrap();
        fs::rename(workspace.root.join("dir"), workspace.root.join("moved")).unwrap();
        symlink(&outside.root, workspace.root.join("dir")).unwrap();
        let failure = arguments.read_target(&context, &target).unwrap_err();

        assert!(!failure.content.contains("outside secret"));
        assert_eq!(
            failure.content,
            format!(
                "{{\"error\":{{\"type\":\"tool_execution_failed\",\"tool_name\":\"read_file\",\"message\":\"read_file requires a regular file\",\"details\":{{\"field\":\"path\",\"path\":\"{}\",\"error\":\"NotRegularFile\"}},\"suggestion\":\"Use glob_files to inspect directory contents, then choose a regular file.\"}}}}",
                target.display()
            )
        );
    }

    #[test]
    fn read_file_validation_keeps_active_path_only_surface() {
        let decoded = ReadFileArgs::decode("{\"path\":\" /tmp/report.PDF \"}").unwrap();
        assert_eq!(decoded.path, "/tmp/report.PDF");
        assert_eq!(decoded.requested_path, " /tmp/report.PDF ");
    }

    #[test]
    fn read_file_trims_leading_and_trailing_whitespace() {
        let workspace = Workspace::new();
        let path = workspace.write("file.txt", "hello\n");
        let args = format!("{{\"path\":\"  {}  \"}}", path.display());

        assert_eq!(
            text(&rootless_context(), &args),
            format!(
                "<path>{}</path>\n<content>\n1\thello\n</content>",
                path.display()
            )
        );
    }

    #[test]
    fn read_file_honors_line_range_fields_and_uses_active_output_shape() {
        let workspace = Workspace::new();
        let path = workspace.write("file.txt", "one\ntwo\nthree\n");
        let args =
            serde_json::json!({ "path": path, "start_line": 2, "line_count": 2 }).to_string();

        assert_eq!(
            text(&rootless_context(), &args),
            format!(
                "<path>{}</path>\n<content>\n2\ttwo\n3\tthree\n... [showing 2 of 3 lines; use start_line/line_count to read more.]\n</content>",
                path.display()
            )
        );
    }

    #[test]
    fn read_file_pads_line_numbers_to_the_last_shown_width() {
        let workspace = Workspace::new();
        workspace.write("ten.txt", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n");

        assert_eq!(
            text(
                &workspace.context(),
                "{\"path\":\"ten.txt\",\"start_line\":9}"
            ),
            "<path>ten.txt</path>\n<content>\n9 \tl9\n10\tl10\n... [showing 2 of 10 lines; use start_line/line_count to read more.]\n</content>"
        );
    }

    #[test]
    fn read_file_omits_binary_content_using_active_success_output() {
        let workspace = Workspace::new();
        let path = workspace.write("binary.txt", b"hello\x00world\n");

        assert_eq!(
            text(&rootless_context(), &path_args(&path)),
            format!(
                "<path>{}</path>\n<content>binary or non-utf8 file omitted (12 bytes)</content>",
                path.display()
            )
        );
    }

    #[test]
    fn read_file_reports_start_line_beyond_file_length() {
        let workspace = Workspace::new();
        let path = workspace.write("file.txt", "one\ntwo\n");
        let args = serde_json::json!({ "path": path, "start_line": 5 }).to_string();

        assert_eq!(
            text(&rootless_context(), &args),
            format!(
                "<path>{}</path>\n<content>\n... [start_line 5 is beyond end of file; total lines 2]\n</content>",
                path.display()
            )
        );
    }

    #[test]
    fn read_file_reports_empty_file_as_success() {
        let workspace = Workspace::new();
        let path = workspace.write("empty.txt", "");

        assert_eq!(
            text(&rootless_context(), &path_args(&path)),
            format!("<path>{}</path>\n<content>\n</content>", path.display())
        );
    }

    #[test]
    fn read_file_reports_capped_snapshots_for_oversized_text_files() {
        let workspace = Workspace::new();
        let path = workspace.write("large.txt", vec![b'x'; MAX_SNAPSHOT_FILE_BYTES + 1]);

        assert!(
            text(&rootless_context(), &path_args(&path))
                .contains("file snapshot was capped before EOF")
        );
    }

    #[test]
    fn read_file_sparse_oversized_files_use_active_byte_cap() {
        let workspace = Workspace::new();
        let path = workspace.root.join("too-large.txt");
        File::create(&path)
            .unwrap()
            .set_len(MAX_SNAPSHOT_FILE_BYTES as u64 + 1)
            .unwrap();

        assert!(
            text(&rootless_context(), &path_args(&path))
                .contains("binary or non-utf8 file omitted")
        );
    }

    #[test]
    fn read_file_materializes_enoent_like_active_tool_error_output() {
        let failure = read(
            &rootless_context(),
            "{\"path\":\"/tmp/fx-core-read-file-missing\"}",
        )
        .unwrap_err();

        assert!(
            failure
                .content
                .contains("\"type\":\"tool_execution_failed\"")
        );
        assert!(failure.content.contains("\"tool_name\":\"read_file\""));
        assert!(failure.content.contains("\"error\":\"FileNotFound\""));
    }

    #[test]
    fn read_file_display_budget_tracks_line_number_width_growth() {
        let mut selection = LineSelection::new(1, MAX_LINE_COUNT, MAX_MODEL_OUTPUT_BYTES);

        assert!(selection.keep_line(9, 0, b"l9"));
        assert!(!selection.display_truncated);
        assert_eq!(selection.budget_width, 1);
        assert_eq!(selection.budget_bytes, rendered_line_bytes(1, 2));

        assert!(selection.keep_line(10, 3, b"l10"));
        assert!(!selection.display_truncated);
        assert_eq!(selection.budget_width, 2);
        assert_eq!(
            selection.budget_bytes,
            rendered_line_bytes(2, 2) + rendered_line_bytes(2, 3)
        );
    }

    #[test]
    fn read_file_truncates_long_lines_and_reports_the_partial_view() {
        let workspace = Workspace::new();
        let path = workspace.write("between-caps.txt", vec![b'a'; MAX_MODEL_OUTPUT_BYTES + 128]);

        let output = text(&rootless_context(), &path_args(&path));

        assert!(output.contains("... (line truncated)\n"));
        assert!(output.ends_with(
            "... [showing 1 of 1 lines; use start_line/line_count to read more.]\n</content>"
        ));
    }

    #[test]
    fn read_file_clips_long_lines_at_a_character_boundary() {
        let workspace = Workspace::new();
        let ascii = "a".repeat(1999);
        workspace.write("accent.txt", format!("{ascii}\u{e9}tail\nsecond\n"));

        assert_eq!(
            text(&workspace.context(), r#"{"path":"accent.txt"}"#),
            format!(
                "<path>accent.txt</path>\n<content>\n1\t{ascii}... (line truncated)\n2\tsecond\n... [showing 2 of 2 lines; use start_line/line_count to read more.]\n</content>"
            )
        );
    }

    #[test]
    fn line_clipping_keeps_whole_characters_up_to_the_cap() {
        let cases = [
            (format!("{}\u{e9}x", "a".repeat(1998)), 2000),
            (format!("{}\u{1f600}", "a".repeat(1998)), 1998),
            (format!("{}\u{1f600}", "a".repeat(1996)), 2000),
            ("\u{e9}".repeat(1001), 2000),
            ("short".to_owned(), 5),
        ];
        for (line, clipped) in cases {
            assert_eq!(
                clip_at_char_boundary(line.as_bytes(), DEFAULT_MAX_READ_FILE_LINE_LEN),
                clipped,
                "{line}"
            );
        }
    }

    #[test]
    fn read_file_caps_default_reads_at_the_line_limit() {
        let workspace = Workspace::new();
        let path = workspace.write("long.txt", long_line_text(DEFAULT_MAX_READ_FILE_LINES + 1));

        assert!(text(&rootless_context(), &path_args(&path)).ends_with(
            "... [showing 400 of 401 lines; use start_line/line_count to read more.]\n</content>"
        ));
        let exact = workspace.write(
            "exact.txt",
            vec!["x"; DEFAULT_MAX_READ_FILE_LINES].join("\n"),
        );
        assert!(
            !text(&rootless_context(), &path_args(&exact)).contains("... [showing"),
            "a file with exactly the default line cap is shown in full"
        );
    }

    #[test]
    fn read_file_reports_whether_the_model_saw_the_whole_file() {
        let workspace = Workspace::new();
        workspace.write("two.txt", "one\ntwo\n");
        workspace.write("empty.txt", "");
        workspace.write("binary.bin", [0_u8, 159, 146, 150]);
        workspace.write("large.txt", vec![b'x'; MAX_SNAPSHOT_FILE_BYTES + 1]);
        workspace.write("wide.txt", "x".repeat(DEFAULT_MAX_READ_FILE_LINE_LEN + 1));
        let tool = ReadFile::with_context(workspace.context());
        let coverage = |arguments: &str| {
            run_tool_with(&tool, arguments, PathAccess::WorkspaceOrExternal)
                .1
                .model_view_covers_full_file
        };
        assert_eq!(coverage(r#"{"path":"two.txt"}"#), Some(true));
        assert_eq!(coverage(r#"{"path":"empty.txt"}"#), Some(true));
        assert_eq!(
            coverage(r#"{"path":"two.txt","start_line":2}"#),
            Some(false)
        );
        assert_eq!(
            coverage(r#"{"path":"two.txt","line_count":1}"#),
            Some(false)
        );
        assert_eq!(coverage(r#"{"path":"binary.bin"}"#), Some(false));
        assert_eq!(coverage(r#"{"path":"large.txt"}"#), Some(false));
        assert_eq!(coverage(r#"{"path":"wide.txt"}"#), Some(false));
        assert_eq!(coverage(r#"{"path":"missing.txt"}"#), None);
    }

    #[test]
    fn read_file_reports_missing_paths_before_reading() {
        let workspace = Workspace::new();
        let tool = ReadFile::with_context(workspace.context());

        let (description, output) = run_tool(&tool, "{\"path\":\" missing.txt\"}");

        assert_eq!(description.title, "Reading  missing.txt");
        assert_eq!(
            output,
            ToolOutput::failure("Path not found:  missing.txt")
                .with_status_detail(ToolStatusDetail::PreflightFailed)
        );
    }

    #[test]
    fn read_file_reports_decode_failures_after_describing_the_call() {
        let workspace = Workspace::new();
        let tool = ReadFile::with_context(workspace.context());

        let (description, output) = run_tool(&tool, "{\"path\":7}");

        assert_eq!(description.title, "Reading file");
        assert_eq!(
            output,
            ToolOutput::failure("read_file field \"path\" must be a string")
        );
    }

    #[test]
    fn read_file_classifiers_are_read_only_and_parallel() {
        let workspace = Workspace::new();
        workspace.write("a.txt", "one\n");
        let tool = ReadFile::with_context(workspace.context());

        let (description, output) = run_tool(&tool, "{\"path\":\"a.txt\"}");

        assert_eq!(
            description,
            CallDescription {
                title: "Reading a.txt".to_owned(),
                label: Some(PRESENTATION.label("a.txt")),
                activity: ToolActivity::Read,
                effect: ToolEffect::ReadOnly,
                concurrency: Concurrency::Parallel,
            }
        );
        assert_eq!(
            output,
            ToolOutput::success("<path>a.txt</path>\n<content>\n1\tone\n</content>")
                .covering_full_file(true)
        );
    }
}
