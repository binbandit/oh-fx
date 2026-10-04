use std::fs;
use std::path::Path;

use ofx_contract::{
    ActionLabel, ApplicableTarget, CallDescription, Concurrency, FileChangeStats, FileMutation,
    FileMutationState, PathAccess, PermissionMode, TargetKind, ToolCallId, ToolContext, ToolEffect,
    ToolStatusDetail,
};
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

    fn write(&self, relative: &str, content: &str) {
        fs::write(self.workspace.join(relative), content).unwrap();
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.workspace.join(relative)).unwrap()
    }

    fn run(&self, arguments: &str) -> Run {
        run(&EditFile::new(&self.workspace), arguments)
    }
}

struct Run {
    description: CallDescription,
    mutation: Option<FileMutation>,
    output: ToolOutput,
}

fn run(tool: &EditFile, arguments: &str) -> Run {
    let mut prepared = tool.prepare(arguments).unwrap();
    prepared.complete();
    let description = prepared.describe();
    let mutation = prepared.file_mutation().cloned();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let output = runtime.block_on(prepared.execute(ToolContext::new(
        ToolCallId::new("call-1"),
        CancellationToken::new(),
        PathAccess::WorkspaceOrExternal,
    )));
    Run {
        description,
        mutation,
        output,
    }
}

fn arguments(path: impl AsRef<Path>, old_string: &str, new_string: &str) -> String {
    serde_json::json!({
        "path": path.as_ref(),
        "old_string": old_string,
        "new_string": new_string,
    })
    .to_string()
}

#[test]
fn edit_file_keeps_the_upstream_name_and_description() {
    let tool = EditFile::new("/");
    assert_eq!(tool.spec().name, "edit_file");
    assert!(
        tool.spec()
            .description
            .contains("replacing one exact old_string occurrence")
    );
}

#[test]
fn invalid_arguments_fail_with_upstream_messages() {
    let fixture = Fixture::new();
    let long_path = "p".repeat(MAX_PATH_BYTES + 1);
    let long_value = "x".repeat(MAX_CONTENT_BYTES + 1);
    let cases = [
        ("{".to_owned(), "edit_file arguments must be valid JSON"),
        ("[]".to_owned(), "edit_file arguments must be an object"),
        (
            r#"{"old_string":"a","new_string":"b"}"#.to_owned(),
            "edit_file requires string field \"path\"",
        ),
        (
            r#"{"path":1,"old_string":"a","new_string":"b"}"#.to_owned(),
            "edit_file field \"path\" must be a string",
        ),
        (
            r#"{"path":"/tmp/x","new_string":"b"}"#.to_owned(),
            "edit_file requires string field \"old_string\"",
        ),
        (
            r#"{"path":"/tmp/x","old_string":1,"new_string":"b"}"#.to_owned(),
            "edit_file field \"old_string\" must be a string",
        ),
        (
            r#"{"path":"/tmp/x","old_string":"a"}"#.to_owned(),
            "edit_file requires string field \"new_string\"",
        ),
        (
            r#"{"path":"/tmp/x","old_string":"a","new_string":1}"#.to_owned(),
            "edit_file field \"new_string\" must be a string",
        ),
        (
            arguments(&long_path, "old", "new"),
            "file mutation preparation failed: path exceeds the preparation limit",
        ),
        (
            arguments("file.txt", &long_value, "new"),
            "edit_file failed: old_string exceeds the 4 MiB preparation limit",
        ),
        (
            arguments("file.txt", "old", &long_value),
            "edit_file failed: new_string exceeds the 4 MiB preparation limit",
        ),
    ];
    for (arguments, expected) in cases {
        let run = fixture.run(&arguments);
        assert_eq!(run.output, ToolOutput::failure(expected), "{expected}");
        assert_eq!(run.description.effect, ToolEffect::None);
    }
}

#[test]
fn the_change_an_approval_shows_holds_the_exact_bytes_the_edit_writes() {
    let fixture = Fixture::new();
    let before = b"one\r\n\xff\xfe two\r\n\x1b[2Jthree".to_vec();
    fs::write(fixture.workspace.join("raw.bin"), &before).unwrap();
    let mut prepared = EditFile::new(&fixture.workspace)
        .prepare(&arguments("raw.bin", "two\r\n", "2\n"))
        .unwrap();
    prepared.complete();
    let shown = prepared.file_change().unwrap().to_proposed();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(prepared.execute(ToolContext::new(
        ToolCallId::new("call-1"),
        CancellationToken::new(),
        PathAccess::WorkspaceOnly,
    )));
    let written = fs::read(fixture.workspace.join("raw.bin")).unwrap();
    assert_eq!(shown.display_path, "raw.bin");
    assert_eq!(shown.before.as_deref(), Some(&before[..]));
    assert_eq!(*shown.after, written[..]);
    assert_eq!(written, b"one\r\n\xff\xfe 2\n\x1b[2Jthree");
}

#[test]
fn one_exact_occurrence_is_replaced() {
    let fixture = Fixture::new();
    fixture.write("note.txt", "alpha\nbeta\ngamma\n");

    let run = fixture.run(&arguments("note.txt", "beta", "BETA"));

    assert_eq!(
        run.description,
        CallDescription {
            title: "Editing note.txt".to_owned(),
            label: Some(ActionLabel {
                active: "Editing",
                completed: "Edited",
                target: "note.txt".to_owned(),
            }),
            activity: ToolActivity::Edit,
            effect: ToolEffect::Irreversible,
            concurrency: Concurrency::Serial,
        }
    );
    assert_eq!(
        run.mutation,
        Some(FileMutation {
            target: fixture.workspace.join("note.txt"),
            state: FileMutationState::Changes,
        })
    );
    assert_eq!(
        run.output,
        ToolOutput::success("edited note.txt (17 bytes)").with_file_change(FileChangeStats {
            additions: 1,
            deletions: 1,
        })
    );
    assert_eq!(fixture.read("note.txt"), "alpha\nBETA\ngamma\n");
}

#[test]
fn failed_preparations_name_no_target_unless_full_access_defers_them() {
    let fixture = Fixture::new();
    fixture.write("note.txt", "alpha\n");
    let cases = [
        (arguments("missing.txt", "a", "b"), "Editing file"),
        (arguments("note.txt", "zz", "b"), "Editing file"),
        ("[]".to_owned(), "Editing file"),
    ];
    for (arguments, expected) in &cases {
        let run = fixture.run(arguments);
        assert_eq!(run.description.title, *expected, "{arguments}");
        assert_eq!(run.description.effect, ToolEffect::None, "{arguments}");
    }
    let live = LivePermissionMode::from(PermissionMode::Auto);
    let full_access = EditFile::new(&fixture.workspace).with_permission_mode(live.clone());
    let prepared = run(&full_access, &arguments("note.txt", "zz", "b"));
    assert_eq!(prepared.description.title, "Editing file");
    live.set(PermissionMode::Yolo);
    let deferred = run(&full_access, &arguments("note.txt", "zz", "b"));
    assert_eq!(deferred.description.title, "Editing note.txt");
    assert_eq!(deferred.description.effect, ToolEffect::Irreversible);
    assert_eq!(
        deferred.output.content,
        "edit_file failed: old_string not found in file. Re-read the file to see its current contents; if the change is already applied, do not retry this edit."
    );
    let missing = run(&full_access, &arguments("missing.txt", "a", "b"));
    assert_eq!(missing.description.title, "Editing file");
    assert_eq!(fixture.read("note.txt"), "alpha\n");
}

#[test]
fn edits_are_checked_against_the_content_read_when_they_complete() {
    let fixture = Fixture::new();
    fixture.write("note.txt", "alpha\n");
    let mut prepared = EditFile::new(&fixture.workspace)
        .prepare(&arguments("note.txt", "beta", "BETA"))
        .unwrap();
    assert_eq!(prepared.describe().effect, ToolEffect::Irreversible);
    fixture.write("note.txt", "beta\n");
    prepared.complete();
    assert_eq!(prepared.describe().title, "Editing note.txt");
    assert_eq!(
        prepared.file_mutation().map(|mutation| mutation.state),
        Some(FileMutationState::Changes)
    );
    let mut stale = EditFile::new(&fixture.workspace)
        .prepare(&arguments("note.txt", "gamma", "GAMMA"))
        .unwrap();
    stale.complete();
    assert_eq!(stale.describe().title, "Editing file");
    assert_eq!(stale.describe().effect, ToolEffect::None);
    assert_eq!(stale.file_mutation(), None);
    assert_eq!(
        stale.applicable_target(),
        Some(ApplicableTarget {
            path: fixture.workspace.join("note.txt"),
            kind: TargetKind::File,
        })
    );
    let mut vanished = EditFile::new(&fixture.workspace)
        .prepare(&arguments("note.txt", "beta", "BETA"))
        .unwrap();
    fs::remove_file(fixture.workspace.join("note.txt")).unwrap();
    vanished.complete();
    assert_eq!(vanished.file_mutation(), None);
    assert_eq!(vanished.applicable_target(), None);
}

#[test]
fn a_second_edit_prepared_before_the_first_ran_reads_the_file_it_finds() {
    let fixture = Fixture::new();
    fixture.write("note.txt", "old\n");
    let tool = EditFile::new(&fixture.workspace);
    let first = tool.prepare(&arguments("note.txt", "old", "new")).unwrap();
    let second = tool
        .prepare(&arguments("note.txt", "old", "newer"))
        .unwrap();
    let execute = |mut prepared: Box<dyn PreparedCall>| {
        prepared.complete();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(prepared.execute(ToolContext::new(
                ToolCallId::new("call-1"),
                CancellationToken::new(),
                PathAccess::WorkspaceOnly,
            )))
    };
    assert_eq!(
        execute(first),
        ToolOutput::success("edited note.txt (4 bytes)").with_file_change(FileChangeStats {
            additions: 1,
            deletions: 1,
        })
    );
    assert_eq!(
        execute(second).content,
        "edit_file failed: old_string not found in file. Re-read the file to see its current contents; if the change is already applied, do not retry this edit."
    );
    assert_eq!(fixture.read("note.txt"), "new\n");
}

#[test]
fn matching_is_exact_and_failures_leave_the_file_untouched() {
    let fixture = Fixture::new();
    let original = "same twice same\r\nline two\r\n";
    fixture.write("note.txt", original);
    let not_found = "edit_file failed: old_string not found in file. Re-read the file to see its current contents; if the change is already applied, do not retry this edit.";
    let cases = [
        (
            "same",
            "same",
            "edit_file failed: old_string and new_string are identical",
        ),
        ("missing", "new", not_found),
        ("", "new", not_found),
        ("line two\n", "new", not_found),
        ("SAME twice", "new", not_found),
        (
            "same",
            "new",
            "edit_file failed: old_string is not unique (found 2 occurrences), provide more context",
        ),
    ];
    for (old_string, new_string, expected) in cases {
        let run = fixture.run(&arguments("note.txt", old_string, new_string));
        assert_eq!(
            run.output,
            ToolOutput::failure(expected).with_status_detail(ToolStatusDetail::PreflightFailed),
            "{old_string:?}"
        );
        assert_eq!(run.description.effect, ToolEffect::None, "{old_string:?}");
        assert_eq!(run.description.title, "Editing file", "{old_string:?}");
        assert_eq!(
            run.description
                .label
                .map(|label| (label.completed, label.target)),
            Some(("Edited", "note.txt".to_owned())),
            "{old_string:?}"
        );
        assert_eq!(run.mutation, None);
    }
    assert_eq!(fixture.read("note.txt"), original);

    let crlf = fixture.run(&arguments("note.txt", "line two\r\n", "line 2\r\n"));
    assert_eq!(
        crlf.output,
        ToolOutput::success("edited note.txt (25 bytes)").with_file_change(FileChangeStats {
            additions: 1,
            deletions: 1,
        })
    );
    assert_eq!(fixture.read("note.txt"), "same twice same\r\nline 2\r\n");
}

#[test]
fn occurrences_are_counted_without_overlap() {
    let fixture = Fixture::new();
    fixture.write("note.txt", "aaa");

    let run = fixture.run(&arguments("note.txt", "aa", "b"));

    assert_eq!(
        run.output,
        ToolOutput::success("edited note.txt (2 bytes)").with_file_change(FileChangeStats {
            additions: 1,
            deletions: 1,
        })
    );
    assert_eq!(fixture.read("note.txt"), "ba");
}

#[test]
fn edits_need_an_existing_regular_file() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.workspace.join("directory")).unwrap();
    for (path, expected) in [
        (
            "missing.txt",
            "file mutation target resolution failed: file_not_found",
        ),
        (
            "missing/file.txt",
            "file mutation target resolution failed: file_not_found",
        ),
        (
            "directory",
            "file mutation preparation failed: target is not a regular file",
        ),
    ] {
        let run = fixture.run(&arguments(path, "old", "new"));
        assert_eq!(
            run.output,
            ToolOutput::failure(expected).with_status_detail(ToolStatusDetail::PreflightFailed),
            "{path}"
        );
    }
    assert!(!fixture.workspace.join("missing").exists());
}

#[test]
fn postimages_over_the_limit_fail_before_approval() {
    let fixture = Fixture::new();
    let mut content = "x".repeat(MAX_CONTENT_BYTES);
    content.replace_range(..1, "a");
    fixture.write("large.txt", &content);

    let run = fixture.run(&arguments("large.txt", "a", "aa"));

    assert_eq!(
        run.output,
        ToolOutput::failure("edit_file failed: postimage exceeds the 4 MiB preparation limit")
            .with_status_detail(ToolStatusDetail::PreflightFailed)
    );
    assert_eq!(fixture.read("large.txt").len(), MAX_CONTENT_BYTES);
}

#[test]
fn edits_in_an_additional_directory_are_read_before_admission() {
    let fixture = Fixture::new();
    let shared = fixture.root.join("shared");
    fs::create_dir_all(&shared).unwrap();
    let notes = shared.join("notes.txt");
    fs::write(&notes, "secret value\n").unwrap();
    let tool = EditFile::new(&fixture.workspace).with_additional_roots(vec![shared]);
    let missing = run(&tool, &arguments(&notes, "missing", "new"));
    assert_eq!(missing.mutation, None);
    assert_eq!(
        missing.output.status,
        ofx_contract::ToolResultStatus::Failure
    );
    let edited = run(&tool, &arguments(&notes, "secret", "public"));
    assert_eq!(
        edited.mutation,
        Some(FileMutation {
            target: notes.clone(),
            state: FileMutationState::Changes,
        })
    );
    assert_eq!(fs::read_to_string(&notes).unwrap(), "public value\n");
}

#[test]
fn external_edits_are_deferred_until_admission() {
    let fixture = Fixture::new();
    let outside = fixture.root.join("outside.txt");
    fs::write(&outside, "secret value\n").unwrap();

    let run = fixture.run(&arguments(&outside, "missing", "new"));

    assert_eq!(
        run.mutation,
        Some(FileMutation {
            target: outside.clone(),
            state: FileMutationState::Unread,
        })
    );
    assert_eq!(run.description.effect, ToolEffect::Irreversible);
    let edited = fixture.run(&arguments(&outside, "secret", "public"));
    assert_eq!(
        edited.output,
        ToolOutput::success(format!("edited {} (13 bytes)", outside.display())).with_file_change(
            FileChangeStats {
                additions: 1,
                deletions: 1,
            }
        )
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), "public value\n");
}

#[test]
fn committed_edits_are_tracked_with_the_content_they_replaced() {
    let workspace = Fixture::new();
    let tracker = ChangeTracker::default();
    let tool = EditFile::new(&workspace.workspace).with_change_tracker(tracker.clone());
    workspace.write("note.txt", "alpha beta\n");

    run(&tool, &arguments("note.txt", "gamma", "delta"));
    assert_eq!(tracker.undo_last(), UndoResult::Empty);

    run(&tool, &arguments("note.txt", "beta", "gamma"));
    assert_eq!(workspace.read("note.txt"), "alpha gamma\n");
    assert_eq!(
        tracker.undo_last(),
        UndoResult::Restored(workspace.workspace.join("note.txt"))
    );
    assert_eq!(workspace.read("note.txt"), "alpha beta\n");
}
