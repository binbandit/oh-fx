use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use ofx_contract::{
    CallDescription, Concurrency, FileMutation, FileMutationState, PathAccess, ToolCallId,
    ToolContext, ToolEffect, ToolResultStatus,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::*;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        Self {
            _temp: temp,
            root,
            workspace,
        }
    }

    fn tool(&self) -> WriteFile {
        WriteFile::new(&self.workspace)
    }
}

struct Run {
    description: CallDescription,
    mutation: Option<FileMutation>,
    output: ToolOutput,
}

fn run(tool: &WriteFile, arguments: &str, path_access: PathAccess) -> Run {
    let prepared = tool.prepare(arguments).unwrap();
    let description = prepared.describe();
    let mutation = prepared.file_mutation().cloned();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let context = ToolContext::new(
        ToolCallId::new("call-1"),
        CancellationToken::new(),
        path_access,
    );
    let output = runtime.block_on(prepared.execute(context));
    Run {
        description,
        mutation,
        output,
    }
}

fn arguments(path: impl AsRef<Path>, content: &str) -> String {
    serde_json::json!({ "path": path.as_ref(), "content": content }).to_string()
}

#[test]
fn write_file_keeps_the_upstream_schema_and_description() {
    let tool = WriteFile::new("/");
    let spec = tool.spec();
    assert_eq!(spec.name, "write_file");
    assert!(spec.description.starts_with("Create or overwrite a file"));
    assert_eq!(spec.input_schema.to_string(), INPUT_SCHEMA);
}

#[test]
fn invalid_arguments_fail_without_touching_the_filesystem() {
    let workspace = Fixture::new();
    let long_path = "p".repeat(MAX_PATH_BYTES + 1);
    let long_content = "x".repeat(MAX_CONTENT_BYTES + 1);
    let cases = [
        ("{".to_owned(), "write_file arguments must be valid JSON"),
        ("[]".to_owned(), "write_file arguments must be an object"),
        (
            r#"{"content":"x"}"#.to_owned(),
            "write_file requires string field \"path\"",
        ),
        (
            r#"{"path":1,"content":"x"}"#.to_owned(),
            "write_file field \"path\" must be a string",
        ),
        (
            r#"{"path":"/tmp/x"}"#.to_owned(),
            "write_file requires string field \"content\"",
        ),
        (
            r#"{"path":"/tmp/x","content":1}"#.to_owned(),
            "write_file field \"content\" must be a string",
        ),
        (arguments(&long_path, "x"), PATH_LIMIT_FAILURE),
        (
            arguments("big.txt", &long_content),
            "write_file failed: content exceeds the 4 MiB preparation limit",
        ),
    ];
    for (arguments, expected) in cases {
        let run = run(&workspace.tool(), &arguments, PathAccess::WorkspaceOnly);
        assert_eq!(run.output, ToolOutput::failure(expected), "{expected}");
        assert_eq!(run.description.effect, ToolEffect::None);
        assert_eq!(run.mutation, None);
    }
    assert_eq!(fs::read_dir(&workspace.workspace).unwrap().count(), 0);
}

#[test]
fn target_and_preparation_failures_are_reported_before_approval() {
    let workspace = Fixture::new();
    fs::write(workspace.workspace.join("file.txt"), "x").unwrap();
    fs::create_dir(workspace.workspace.join("directory")).unwrap();
    let cases = [
        (
            "file.txt/child.txt",
            "file mutation target resolution failed: not_directory",
        ),
        (".", "file mutation target resolution failed: invalid_path"),
        (
            "~other/x",
            "file mutation target resolution failed: invalid_path",
        ),
        (
            "directory",
            "file mutation preparation failed: target is not a regular file",
        ),
    ];
    for (path, expected) in cases {
        let run = run(
            &workspace.tool(),
            &arguments(path, "new"),
            PathAccess::WorkspaceOrExternal,
        );
        assert_eq!(run.output, ToolOutput::failure(expected), "{path}");
        assert_eq!(run.description.effect, ToolEffect::None, "{path}");
        assert_eq!(run.mutation, None, "{path}");
    }
}

#[test]
fn workspace_writes_describe_their_target_and_write_it() {
    let workspace = Fixture::new();
    let run = run(
        &workspace.tool(),
        &arguments("src/new.rs", "fn main() {}\n"),
        PathAccess::WorkspaceOnly,
    );

    assert_eq!(
        run.description,
        CallDescription {
            title: "Writing src/new.rs".to_owned(),
            activity: ToolActivity::Write,
            effect: ToolEffect::Irreversible,
            concurrency: Concurrency::Serial,
        }
    );
    assert_eq!(
        run.mutation,
        Some(FileMutation {
            target: workspace.workspace.join("src/new.rs"),
            state: FileMutationState::Creates,
        })
    );
    assert_eq!(
        run.output,
        ToolOutput::success("wrote src/new.rs (13 bytes)")
    );
    assert_eq!(
        fs::read_to_string(workspace.workspace.join("src/new.rs")).unwrap(),
        "fn main() {}\n"
    );
}

#[test]
fn existing_and_unchanged_workspace_files_are_described_by_their_effect() {
    let workspace = Fixture::new();
    fs::write(workspace.workspace.join("note.txt"), "same\n").unwrap();

    let unchanged = run(
        &workspace.tool(),
        &arguments("note.txt", "same\n"),
        PathAccess::WorkspaceOnly,
    );
    assert_eq!(
        unchanged.mutation.map(|mutation| mutation.state),
        Some(FileMutationState::Unchanged)
    );
    assert_eq!(
        unchanged.output,
        ToolOutput::success("No changes to note.txt; it already contains the requested content")
    );

    let changed = run(
        &workspace.tool(),
        &arguments("note.txt", "different\n"),
        PathAccess::WorkspaceOnly,
    );
    assert_eq!(
        changed.mutation.map(|mutation| mutation.state),
        Some(FileMutationState::Changes)
    );
    assert_eq!(
        changed.output,
        ToolOutput::success("wrote note.txt (10 bytes)")
    );
}

#[test]
fn external_files_are_not_read_until_the_write_is_admitted() {
    let workspace = Fixture::new();
    let existing = workspace.root.join("outside/notes.txt");
    fs::create_dir_all(existing.parent().unwrap()).unwrap();
    fs::write(&existing, "secret\n").unwrap();
    let created = workspace.root.join("outside/new.txt");

    let held = run(
        &workspace.tool(),
        &arguments(&existing, "secret\n"),
        PathAccess::WorkspaceOnly,
    );
    assert_eq!(
        held.mutation,
        Some(FileMutation {
            target: existing.clone(),
            state: FileMutationState::Unread,
        })
    );
    assert_eq!(
        held.output,
        ToolOutput::failure("file mutation target resolution failed: path_outside_workspace")
    );

    let new = run(
        &workspace.tool(),
        &arguments("../outside/new.txt", "fresh\n"),
        PathAccess::WorkspaceOnly,
    );
    assert_eq!(
        new.mutation.map(|mutation| mutation.state),
        Some(FileMutationState::Creates)
    );
    assert!(!created.exists());

    let admitted = run(
        &workspace.tool(),
        &arguments(&existing, "secret\n"),
        PathAccess::WorkspaceOrExternal,
    );
    assert_eq!(
        admitted.output,
        ToolOutput::success(format!(
            "No changes to {}; it already contains the requested content",
            existing.display()
        ))
    );
    let written = run(
        &workspace.tool(),
        &arguments("../outside/new.txt", "fresh\n"),
        PathAccess::WorkspaceOrExternal,
    );
    assert_eq!(
        written.output,
        ToolOutput::success(format!("wrote {} (6 bytes)", created.display()))
    );
    assert_eq!(fs::read_to_string(created).unwrap(), "fresh\n");
}

#[test]
fn deferred_external_writes_refuse_a_path_that_resolves_elsewhere_by_execution() {
    let workspace = Fixture::new();
    fs::create_dir_all(workspace.root.join("first")).unwrap();
    fs::create_dir_all(workspace.root.join("second")).unwrap();
    symlink(workspace.root.join("first"), workspace.root.join("link")).unwrap();
    let requested = workspace.root.join("link/new.txt");

    let prepared = workspace
        .tool()
        .prepare(&arguments(&requested, "new\n"))
        .unwrap();
    fs::remove_file(workspace.root.join("link")).unwrap();
    symlink(workspace.root.join("second"), workspace.root.join("link")).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let output = runtime.block_on(prepared.execute(ToolContext::new(
        ToolCallId::new("call-1"),
        CancellationToken::new(),
        PathAccess::WorkspaceOrExternal,
    )));

    assert_eq!(
        output,
        ToolOutput::failure(
            "file mutation preparation failed: approved target no longer matches the call"
        )
    );
    assert!(!workspace.root.join("first/new.txt").exists());
    assert!(!workspace.root.join("second/new.txt").exists());
}

#[test]
fn a_new_external_file_that_appears_before_execution_is_not_overwritten() {
    let workspace = Fixture::new();
    let target = workspace.root.join("outside/new.txt");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    let prepared = workspace
        .tool()
        .prepare(&arguments(&target, "mine\n"))
        .unwrap();
    assert_eq!(
        prepared.file_mutation().map(|mutation| mutation.state),
        Some(FileMutationState::Creates)
    );
    fs::write(&target, "theirs\n").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let output = runtime.block_on(prepared.execute(ToolContext::new(
        ToolCallId::new("call-1"),
        CancellationToken::new(),
        PathAccess::WorkspaceOrExternal,
    )));

    assert_eq!(
        output,
        ToolOutput::failure(
            "file mutation preparation failed: approved filesystem identity changed"
        )
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "theirs\n");
}

#[test]
fn cancelled_writes_report_cancellation_and_leave_no_file() {
    let workspace = Fixture::new();
    let prepared = workspace
        .tool()
        .prepare(&arguments("new.txt", "new\n"))
        .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let output = runtime.block_on(prepared.execute(ToolContext::new(
        ToolCallId::new("call-1"),
        cancel,
        PathAccess::WorkspaceOnly,
    )));

    assert_eq!(
        output,
        ToolOutput::failure("file mutation cancelled before commit")
    );
    assert_eq!(output.status, ToolResultStatus::Failure);
    assert!(!workspace.workspace.join("new.txt").exists());
}
