use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use ofx_contract::{
    ApplicableTarget, CallDescription, Concurrency, FileMutation, FileMutationState, PathAccess,
    PermissionMode, TargetKind, ToolCallId, ToolContext, ToolEffect, ToolResultStatus,
};
use ofx_workspace::MAX_PATH_BYTES;
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
    let mut prepared = tool.prepare(arguments).unwrap();
    prepared.complete();
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
fn write_file_keeps_the_upstream_name_and_description() {
    let tool = WriteFile::new("/");
    let spec = tool.spec();
    assert_eq!(spec.name, "write_file");
    assert!(spec.description.starts_with("Create or overwrite a file"));
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
        (
            arguments(&long_path, "x"),
            "file mutation preparation failed: path exceeds the preparation limit",
        ),
        (
            arguments("big.txt", &long_content),
            "write_file failed: content exceeds the 4 MiB preparation limit",
        ),
    ];
    for (arguments, expected) in cases {
        let run = run(&workspace.tool(), &arguments, PathAccess::WorkspaceOnly);
        assert_eq!(run.output, ToolOutput::failure(expected), "{expected}");
        assert_eq!(run.description.title, "Writing file", "{expected}");
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
        assert_eq!(run.description.title, "Writing file", "{path}");
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

fn execute_after(tool: &WriteFile, arguments: &str, change: impl FnOnce()) -> Run {
    let mut prepared = tool.prepare(arguments).unwrap();
    prepared.complete();
    let description = prepared.describe();
    let mutation = prepared.file_mutation().cloned();
    change();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let output = runtime.block_on(prepared.execute(ToolContext::new(
        ToolCallId::new("call-1"),
        CancellationToken::new(),
        PathAccess::WorkspaceOnly,
    )));
    Run {
        description,
        mutation,
        output,
    }
}

#[test]
fn an_unchanged_file_that_changes_before_execution_is_reported_stale_and_left_alone() {
    let stale = "file mutation rejected because the file changed after preview; make a new tool call for a fresh preview";
    let workspace = Fixture::new();
    let path = workspace.workspace.join("note.txt");
    fs::write(&path, "same\n").unwrap();

    let changed = execute_after(&workspace.tool(), &arguments("note.txt", "same\n"), || {
        fs::write(&path, "edited elsewhere\n").unwrap();
    });
    assert_eq!(
        changed.mutation.map(|mutation| mutation.state),
        Some(FileMutationState::Unchanged)
    );
    assert_eq!(changed.output, ToolOutput::failure(stale));
    assert_eq!(fs::read_to_string(&path).unwrap(), "edited elsewhere\n");

    fs::write(&path, "same\n").unwrap();
    let deleted = execute_after(&workspace.tool(), &arguments("note.txt", "same\n"), || {
        fs::remove_file(&path).unwrap();
    });
    assert_eq!(deleted.output, ToolOutput::failure(stale));
    assert!(!path.exists());
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
fn progress_titles_name_the_prepared_target() {
    let workspace = Fixture::new();
    fs::create_dir_all(workspace.root.join("outside")).unwrap();
    let tool = workspace.tool();
    let cases = [
        ("a\tb.txt".to_owned(), "Writing a\\x09b.txt".to_owned()),
        (
            "../outside/new.txt".to_owned(),
            format!(
                "Writing {}",
                workspace.root.join("outside/new.txt").display()
            ),
        ),
    ];
    for (path, expected) in cases {
        let mut prepared = tool.prepare(&arguments(&path, "x")).unwrap();
        prepared.complete();
        assert_eq!(prepared.describe().title, expected, "{path}");
        assert_eq!(prepared.untargeted_title(), "Writing file", "{path}");
    }
}

#[test]
fn full_access_writes_name_the_requested_path_and_read_the_target_when_they_run() {
    let workspace = Fixture::new();
    let tool = workspace
        .tool()
        .with_permission_mode(PermissionMode::Yolo.into());
    fs::write(workspace.workspace.join("note.txt"), "old\n").unwrap();
    let mut prepared = tool.prepare(&arguments("./note.txt", "new\n")).unwrap();
    prepared.complete();
    assert_eq!(prepared.describe().title, "Writing ./note.txt");
    assert_eq!(
        prepared.file_mutation().cloned(),
        Some(FileMutation {
            target: workspace.workspace.join("note.txt"),
            state: FileMutationState::Unread,
        })
    );
    fs::write(workspace.workspace.join("note.txt"), "new\n").unwrap();
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
        ToolOutput::success("No changes to ./note.txt; it already contains the requested content")
    );
    let failed = run(
        &tool,
        &arguments("note.txt/x.txt", "x"),
        PathAccess::WorkspaceOrExternal,
    );
    assert_eq!(failed.description.title, "Writing file");
}

#[test]
fn file_changes_keep_their_prepared_target_until_completion_resolves_and_reads_it_again() {
    let workspace = Fixture::new();
    let directory = workspace.workspace.join("sub");
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("note.txt"), "old\n").unwrap();
    let mut prepared = workspace
        .tool()
        .prepare(&arguments("sub/note.txt", "new\n"))
        .unwrap();
    let target = Some(ApplicableTarget {
        path: directory.join("note.txt"),
        kind: TargetKind::File,
    });
    assert_eq!(
        prepared.file_mutation().map(|mutation| mutation.state),
        Some(FileMutationState::Unread)
    );
    assert_eq!(prepared.applicable_target(), target);
    fs::rename(&directory, workspace.workspace.join("moved")).unwrap();
    fs::write(&directory, "now a file\n").unwrap();
    assert_eq!(prepared.applicable_target(), target);
    fs::remove_file(&directory).unwrap();
    fs::rename(workspace.workspace.join("moved"), &directory).unwrap();
    fs::write(directory.join("note.txt"), "new\n").unwrap();
    prepared.complete();
    assert_eq!(
        prepared.file_mutation().map(|mutation| mutation.state),
        Some(FileMutationState::Unchanged)
    );
    assert_eq!(prepared.describe().title, "Writing sub/note.txt");
    assert_eq!(prepared.applicable_target(), target);
    let mut stranded = workspace
        .tool()
        .prepare(&arguments("sub/note.txt", "newer\n"))
        .unwrap();
    fs::remove_dir_all(&directory).unwrap();
    fs::write(&directory, "now a file\n").unwrap();
    stranded.complete();
    assert_eq!(stranded.applicable_target(), None);
    assert_eq!(stranded.file_mutation(), None);
    assert_eq!(stranded.describe().effect, ToolEffect::None);
}

#[test]
fn completion_reports_the_target_a_retargeted_path_now_resolves_to() {
    let workspace = Fixture::new();
    fs::create_dir_all(workspace.workspace.join("first")).unwrap();
    fs::create_dir_all(workspace.workspace.join("second")).unwrap();
    symlink("first", workspace.workspace.join("link")).unwrap();
    let mut prepared = workspace
        .tool()
        .prepare(&arguments("link/new.txt", "new\n"))
        .unwrap();
    let prepared_target = workspace.workspace.join("first/new.txt");
    assert_eq!(
        prepared.applicable_target().map(|target| target.path),
        Some(prepared_target)
    );
    fs::remove_file(workspace.workspace.join("link")).unwrap();
    symlink("second", workspace.workspace.join("link")).unwrap();
    prepared.complete();
    let completed_target = workspace.workspace.join("second/new.txt");
    assert_eq!(
        prepared.applicable_target().map(|target| target.path),
        Some(completed_target.clone())
    );
    assert_eq!(
        prepared
            .file_mutation()
            .map(|mutation| mutation.target.clone()),
        Some(completed_target)
    );
}

#[test]
fn a_call_executed_without_completion_refuses_a_path_that_now_resolves_elsewhere() {
    let workspace = Fixture::new();
    fs::create_dir_all(workspace.workspace.join("first")).unwrap();
    fs::create_dir_all(workspace.workspace.join("second")).unwrap();
    symlink("first", workspace.workspace.join("link")).unwrap();
    let prepared = workspace
        .tool()
        .prepare(&arguments("link/new.txt", "new\n"))
        .unwrap();
    fs::remove_file(workspace.workspace.join("link")).unwrap();
    symlink("second", workspace.workspace.join("link")).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let output = runtime.block_on(prepared.execute(ToolContext::new(
        ToolCallId::new("call-1"),
        CancellationToken::new(),
        PathAccess::WorkspaceOnly,
    )));
    assert_eq!(
        output,
        ToolOutput::failure(
            "file mutation preparation failed: approved target no longer matches the call"
        )
    );
    assert!(!workspace.workspace.join("first/new.txt").exists());
    assert!(!workspace.workspace.join("second/new.txt").exists());
}

#[test]
fn deferred_external_writes_refuse_a_path_that_resolves_elsewhere_by_execution() {
    let workspace = Fixture::new();
    fs::create_dir_all(workspace.root.join("first")).unwrap();
    fs::create_dir_all(workspace.root.join("second")).unwrap();
    symlink(workspace.root.join("first"), workspace.root.join("link")).unwrap();
    let requested = workspace.root.join("link/new.txt");

    let mut prepared = workspace
        .tool()
        .prepare(&arguments(&requested, "new\n"))
        .unwrap();

    prepared.complete();
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
    let mut prepared = workspace
        .tool()
        .prepare(&arguments(&target, "mine\n"))
        .unwrap();
    prepared.complete();
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
    let mut prepared = workspace
        .tool()
        .prepare(&arguments("new.txt", "new\n"))
        .unwrap();
    prepared.complete();
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
