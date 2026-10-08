use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::OnceLock;

use ofx_contract::{
    ActionLabel, Admission, ApplicableTarget, CallDescription, Concurrency, FileChange,
    FileChangeStats, FileMutation, FileMutationState, LiveAdditionalRoots, PathAccess,
    PermissionGate, PermissionMode, TargetKind, ToolCallId, ToolContext, ToolEffect,
    ToolResultStatus, ToolStatusDetail,
};
use ofx_permissions::PermissionPolicy;
use ofx_workspace::{MAX_PATH_BYTES, UndoResult};
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
        assert_eq!(
            run.description.label.map(|label| label.target),
            Some("file".to_owned()),
            "{expected}"
        );
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
        assert_eq!(
            run.output,
            ToolOutput::failure(expected).with_status_detail(ToolStatusDetail::PreflightFailed),
            "{path}"
        );
        assert_eq!(run.description.title, "Writing file", "{path}");
        assert_eq!(
            run.description.label.map(|label| label.target),
            Some(path.to_owned()),
            "{path}"
        );
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
            label: Some(ActionLabel {
                active: "Writing",
                completed: "Wrote",
                target: "src/new.rs".to_owned(),
            }),
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
        ToolOutput::success("wrote src/new.rs (13 bytes)").with_file_change(FileChangeStats {
            additions: 1,
            deletions: 0,
        })
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
        ToolOutput::success("wrote note.txt (10 bytes)").with_file_change(FileChangeStats {
            additions: 1,
            deletions: 1,
        })
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
    assert_eq!(
        changed.output,
        ToolOutput::failure(stale).with_status_detail(ToolStatusDetail::StalePreview)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "edited elsewhere\n");

    fs::write(&path, "same\n").unwrap();
    let deleted = execute_after(&workspace.tool(), &arguments("note.txt", "same\n"), || {
        fs::remove_file(&path).unwrap();
    });
    assert_eq!(
        deleted.output,
        ToolOutput::failure(stale).with_status_detail(ToolStatusDetail::StalePreview)
    );
    assert!(!path.exists());
}

#[test]
fn files_in_an_additional_directory_are_read_before_the_write_is_admitted() {
    let workspace = Fixture::new();
    let shared = workspace.root.join("shared");
    fs::create_dir_all(&shared).unwrap();
    let existing = shared.join("notes.txt");
    fs::write(&existing, "old\n").unwrap();
    let outside = workspace.root.join("outside.txt");
    fs::write(&outside, "old\n").unwrap();
    let tool = workspace.tool().with_additional_roots(vec![shared.clone()]);
    let changed = run(
        &tool,
        &arguments(&existing, "new\n"),
        PathAccess::Within(shared.clone()),
    );
    assert_eq!(
        changed.mutation,
        Some(FileMutation {
            target: existing.clone(),
            state: FileMutationState::Changes,
        })
    );
    assert_eq!(changed.output.status, ToolResultStatus::Success);
    assert_eq!(fs::read_to_string(&existing).unwrap(), "new\n");
    let unchanged = run(
        &tool,
        &arguments(&existing, "new\n"),
        PathAccess::Within(shared.clone()),
    );
    assert_eq!(
        unchanged.mutation.map(|mutation| mutation.state),
        Some(FileMutationState::Unchanged)
    );
    let elsewhere = run(
        &tool,
        &arguments(&outside, "new\n"),
        PathAccess::WorkspaceOnly,
    );
    assert_eq!(
        elsewhere.mutation.map(|mutation| mutation.state),
        Some(FileMutationState::Unread)
    );
}

#[test]
fn an_additional_directory_installed_after_the_tool_is_built_is_read_before_admission() {
    let workspace = Fixture::new();
    let shared = workspace.root.join("shared");
    fs::create_dir_all(&shared).unwrap();
    let existing = shared.join("notes.txt");
    fs::write(&existing, "old\n").unwrap();
    let roots = LiveAdditionalRoots::default();
    let tool = workspace.tool().with_additional_roots(roots.clone());
    let unread = run(
        &tool,
        &arguments(&existing, "old\n"),
        PathAccess::WorkspaceOnly,
    );
    assert_eq!(
        unread.mutation.map(|mutation| mutation.state),
        Some(FileMutationState::Unread)
    );
    roots.set(vec![shared.clone()]);
    let unchanged = run(
        &tool,
        &arguments(&existing, "old\n"),
        PathAccess::Within(shared.clone()),
    );
    assert_eq!(
        unchanged.mutation.map(|mutation| mutation.state),
        Some(FileMutationState::Unchanged)
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
            .with_status_detail(ToolStatusDetail::PreflightFailed)
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
        ToolOutput::success(format!("wrote {} (6 bytes)", created.display())).with_file_change(
            FileChangeStats {
                additions: 1,
                deletions: 0,
            }
        )
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
        assert_eq!(
            prepared.untargeted_label(),
            Some(ActionLabel {
                active: "Writing",
                completed: "Wrote",
                target: "file".to_owned(),
            }),
            "{path}"
        );
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
fn a_write_is_staged_in_the_mode_in_force_when_it_completes() {
    let workspace = Fixture::new();
    let target = workspace.workspace.join("note.txt");
    fs::write(&target, "old\n").unwrap();
    let mode = LivePermissionMode::from(PermissionMode::Yolo);
    let tool = workspace.tool().with_permission_mode(mode.clone());
    let policy = PermissionPolicy::new(mode.clone(), &workspace.workspace);
    let cases = [
        (
            PermissionMode::Yolo,
            PermissionMode::Auto,
            FileMutationState::Changes,
            Admission::Allowed(PathAccess::WorkspaceOnly),
        ),
        (
            PermissionMode::Auto,
            PermissionMode::Yolo,
            FileMutationState::Unread,
            Admission::Allowed(PathAccess::WorkspaceOrExternal),
        ),
    ];
    for (prepared_in, completed_in, state, admission) in cases {
        mode.set(prepared_in);
        let mut prepared = tool.prepare(&arguments("./note.txt", "new\n")).unwrap();
        mode.set(completed_in);
        prepared.complete();
        let mutation = prepared.file_mutation().cloned().unwrap();
        assert_eq!(
            mutation,
            FileMutation {
                target: target.clone(),
                state,
            },
            "{completed_in:?}"
        );
        assert_eq!(
            policy.admit_file_mutation(&mutation),
            admission,
            "{completed_in:?}"
        );
    }
}

#[test]
fn a_completed_write_keeps_its_staging_when_the_mode_changes_before_it_runs() {
    let workspace = Fixture::new();
    let path = workspace.workspace.join("note.txt");
    fs::write(&path, "old\n").unwrap();
    let mode = LivePermissionMode::from(PermissionMode::Auto);
    let tool = workspace.tool().with_permission_mode(mode.clone());
    let changed = execute_after(&tool, &arguments("note.txt", "new\n"), || {
        mode.set(PermissionMode::Yolo);
        fs::write(&path, "edited elsewhere\n").unwrap();
    });
    assert_eq!(
        changed.mutation.map(|mutation| mutation.state),
        Some(FileMutationState::Changes)
    );
    assert_eq!(
        changed.output,
        ToolOutput::failure(
            "file mutation rejected because the file changed after preview; make a new tool call for a fresh preview"
        )
        .with_status_detail(ToolStatusDetail::StalePreview)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "edited elsewhere\n");
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
        .with_status_detail(ToolStatusDetail::Rejected)
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
        .with_status_detail(ToolStatusDetail::Rejected)
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
        .with_status_detail(ToolStatusDetail::PreflightFailed)
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
            .with_status_detail(ToolStatusDetail::Cancelled)
    );
    assert_eq!(output.status, ToolResultStatus::Failure);
    assert!(!workspace.workspace.join("new.txt").exists());
}

#[test]
fn prepared_changes_show_the_reviewer_their_exact_content_unless_they_would_read_outside() {
    let workspace = Fixture::new();
    fs::create_dir_all(workspace.workspace.join(".git")).unwrap();
    fs::write(workspace.workspace.join(".git/config"), "[core]\n").unwrap();
    let mut changed = workspace
        .tool()
        .prepare(&arguments(".git/config", "[core]\n\thooksPath = /tmp/x\n"))
        .unwrap();
    assert_eq!(changed.file_change(), None);
    changed.complete();
    assert_eq!(
        changed.file_change(),
        Some(FileChange {
            display_path: ".git/config".to_owned(),
            before: Some(b"[core]\n"),
            after: b"[core]\n\thooksPath = /tmp/x\n",
            parents: vec![workspace.workspace.join(".git")],
            line_counts: Some(&OnceLock::new()),
        })
    );
    let mut created = workspace
        .tool()
        .prepare(&arguments("a/b/new.txt", "new\n"))
        .unwrap();
    created.complete();
    assert_eq!(
        created.file_change(),
        Some(FileChange {
            display_path: "a/b/new.txt".to_owned(),
            before: None,
            after: b"new\n",
            parents: vec![
                workspace.workspace.join("a/b"),
                workspace.workspace.join("a")
            ],
            line_counts: Some(&OnceLock::new()),
        })
    );
    let external = workspace.root.join("outside.txt");
    fs::write(&external, "outside secret\n").unwrap();
    let mut outside = workspace
        .tool()
        .prepare(&arguments(external.to_str().unwrap(), "replaced\n"))
        .unwrap();
    outside.complete();
    assert_eq!(
        outside.file_mutation().map(|mutation| mutation.state),
        Some(FileMutationState::Unread)
    );
    assert_eq!(outside.file_change(), None);
    let mut fresh = workspace
        .tool()
        .prepare(&arguments(
            workspace.root.join("fresh/new.txt").to_str().unwrap(),
            "fresh\n",
        ))
        .unwrap();
    fresh.complete();
    assert_eq!(
        fresh.file_mutation().map(|mutation| mutation.state),
        Some(FileMutationState::Creates)
    );
    let change = fresh
        .file_change()
        .expect("a new external file is prepared");
    assert_eq!((change.before, change.after), (None, &b"fresh\n"[..]));
    assert!(
        change.display_path.ends_with("/fresh/new.txt"),
        "{change:?}"
    );
}

#[test]
fn a_reviewed_change_reports_the_line_counts_its_review_recorded() {
    let workspace = Fixture::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let write = |reviewed: Option<FileChangeStats>| {
        fs::write(workspace.workspace.join("note.txt"), "old\n").unwrap();
        let mut prepared = workspace
            .tool()
            .prepare(&arguments("note.txt", "new\n"))
            .unwrap();
        prepared.complete();
        if let Some(counts) = reviewed {
            let change = prepared.file_change().unwrap();
            change.line_counts.unwrap().set(counts).unwrap();
        }
        runtime
            .block_on(prepared.execute(ToolContext::new(
                ToolCallId::new("call-1"),
                CancellationToken::new(),
                PathAccess::WorkspaceOnly,
            )))
            .file_change
    };
    let recorded = FileChangeStats {
        additions: 7,
        deletions: 3,
    };
    assert_eq!(write(Some(recorded)), Some(recorded));
    assert_eq!(
        write(None),
        Some(FileChangeStats {
            additions: 1,
            deletions: 1,
        })
    );
    assert_eq!(
        fs::read_to_string(workspace.workspace.join("note.txt")).unwrap(),
        "new\n"
    );
}

#[test]
fn committed_writes_are_tracked_with_the_content_they_replaced() {
    let workspace = Fixture::new();
    let tracker = ChangeTracker::default();
    let tool = workspace.tool().with_change_tracker(tracker.clone());
    let path = workspace.workspace.join("note.txt");
    fs::write(&path, "before\n").unwrap();

    run(
        &tool,
        &arguments("note.txt", "before\n"),
        PathAccess::WorkspaceOnly,
    );
    assert_eq!(tracker.undo_last(), UndoResult::Empty);

    run(
        &tool,
        &arguments("note.txt", "after\n"),
        PathAccess::WorkspaceOnly,
    );
    run(
        &tool,
        &arguments("new.txt", "new\n"),
        PathAccess::WorkspaceOnly,
    );
    let created = workspace.workspace.join("new.txt");
    assert_eq!(tracker.undo_last(), UndoResult::Deleted(created.clone()));
    assert!(!created.exists());
    assert_eq!(tracker.undo_last(), UndoResult::Restored(path.clone()));
    assert_eq!(fs::read_to_string(&path).unwrap(), "before\n");
    assert_eq!(tracker.undo_last(), UndoResult::Empty);
}

#[test]
fn writes_that_never_commit_are_not_tracked() {
    let workspace = Fixture::new();
    let tracker = ChangeTracker::default();
    let tool = workspace.tool().with_change_tracker(tracker.clone());
    let mut prepared = tool.prepare(&arguments("new.txt", "new\n")).unwrap();
    prepared.complete();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(prepared.execute(ToolContext::new(
        ToolCallId::new("call-1"),
        cancel,
        PathAccess::WorkspaceOnly,
    )));
    assert_eq!(tracker.undo_last(), UndoResult::Empty);
}

#[test]
fn undo_refuses_a_created_file_whose_created_parent_was_swapped_for_a_symlink() {
    let workspace = Fixture::new();
    let tracker = ChangeTracker::default();
    let tool = workspace.tool().with_change_tracker(tracker.clone());
    let outside = workspace.root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("new.txt"), "outside bytes\n").unwrap();

    run(
        &tool,
        &arguments("a/b/new.txt", "new\n"),
        PathAccess::WorkspaceOnly,
    );
    let created = workspace.workspace.join("a/b/new.txt");
    assert_eq!(fs::read_to_string(&created).unwrap(), "new\n");
    fs::rename(
        workspace.workspace.join("a/b"),
        workspace.workspace.join("a/kept"),
    )
    .unwrap();
    symlink(&outside, workspace.workspace.join("a/b")).unwrap();

    assert_eq!(tracker.undo_last(), UndoResult::Unavailable(created));
    assert_eq!(
        fs::read_to_string(outside.join("new.txt")).unwrap(),
        "outside bytes\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.workspace.join("a/kept/new.txt")).unwrap(),
        "new\n"
    );
}
