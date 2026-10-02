use std::fs;
use std::os::unix::fs::symlink;

use ofx_contract::{Tool, ToolActivity, ToolCallId, ToolEffect, ToolResultStatus};
use ofx_exec::SessionSupervisor;
use tempfile::TempDir;

use super::*;

const LAUNCHED: &str = r#""state":"lost""#;

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn shell() -> Shell {
    shell_in(std::env::temp_dir())
}

fn shell_in(workspace_root: impl Into<PathBuf>) -> Shell {
    Shell::new(
        workspace_root,
        ManagedExecutions::new(SessionSupervisor::new("/nonexistent")),
        None,
    )
}

fn execute(prepared: Box<dyn PreparedCall>, path_access: PathAccess) -> ToolOutput {
    block_on(prepared.execute(ToolContext::new(
        ToolCallId::new("call-1"),
        CancellationToken::new(),
        path_access,
    )))
}

struct Directories {
    _temp: TempDir,
    workspace: PathBuf,
    build: PathBuf,
    outside: PathBuf,
}

impl Directories {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let directories = Self {
            _temp: temp,
            workspace: root.join("workspace"),
            build: root.join("external/build"),
            outside: root.join("outside"),
        };
        for directory in [
            &directories.workspace,
            &directories.build,
            &directories.outside,
        ] {
            fs::create_dir_all(directory).unwrap();
        }
        directories
    }

    fn run_in_build(&self) -> Box<dyn PreparedCall> {
        let mut prepared = shell_in(&self.workspace)
            .prepare(
                &serde_json::json!({"action": "run", "command": "rm -f marker", "cwd": self.build})
                    .to_string(),
            )
            .unwrap();
        prepared.complete();
        prepared
    }

    fn move_build_away(&self) {
        fs::rename(&self.build, self.build.with_file_name("reviewed")).unwrap();
    }
}

fn description(arguments: &str) -> CallDescription {
    shell().prepare(arguments).unwrap().describe()
}

#[test]
fn the_process_only_schema_matches_upstream_ask_without_a_saved_session() {
    let spec = shell().spec().clone();
    assert_eq!(spec.name, "shell");
    assert_eq!(spec.description, DESCRIPTION);
    let schema: serde_json::Value = serde_json::from_str(spec.input_schema).unwrap();
    let alternatives = schema["properties"]["request"]["oneOf"].as_array().unwrap();
    let actions: Vec<&str> = alternatives
        .iter()
        .map(|schema| schema["properties"]["action"]["enum"][0].as_str().unwrap())
        .collect();
    assert_eq!(actions, ["run", "interact", "stop"]);
    for field in ["tty", "shell", "chars"] {
        assert!(!INPUT_SCHEMA.contains(&format!("\"{field}\":")), "{field}");
    }
}

#[test]
fn shell_calls_run_one_at_a_time_and_classify_their_effects() {
    for (arguments, effect) in [
        (
            r#"{"request":{"action":"run","command":"ls"}}"#,
            ToolEffect::Mutating,
        ),
        (
            r#"{"request":{"action":"interact","session_id":"shell-1"}}"#,
            ToolEffect::ReadOnly,
        ),
        (
            r#"{"request":{"action":"interact","session_id":"shell-1","chars":"y"}}"#,
            ToolEffect::Mutating,
        ),
        (
            r#"{"request":{"action":"stop","session_id":"shell-1"}}"#,
            ToolEffect::Mutating,
        ),
        (r#"{"request":{"action":"run"}}"#, ToolEffect::None),
        ("{", ToolEffect::None),
    ] {
        let description = description(arguments);
        assert_eq!(description.effect, effect, "{arguments}");
        assert_eq!(description.activity, ToolActivity::Command);
        assert_eq!(description.concurrency, Concurrency::Serial);
    }
    assert_eq!(
        description(r#"{"request":{"action":"run","command":"ls -la"}}"#).title,
        "Running ls -la"
    );
}

#[test]
fn shell_calls_describe_the_decoded_request_the_gate_decides() {
    let request = |arguments: &str| {
        shell()
            .prepare(arguments)
            .unwrap()
            .command_request()
            .cloned()
    };
    let run = |profile, terminal| {
        Some(CommandRequest::Run {
            command: "git status".to_owned(),
            cwd: std::env::temp_dir(),
            profile,
            shell: None,
            terminal,
        })
    };
    for (arguments, expected) in [
        (
            r#"{"request":{"action":"run","command":"git status","cwd":"."}}"#,
            run(CommandProfile::User, false),
        ),
        (
            r#"{"action":"run","command":"git status","profile":"user"}"#,
            run(CommandProfile::User, false),
        ),
        (
            r#"{"action":"run","command":"git status","profile":"clean"}"#,
            run(CommandProfile::Clean, false),
        ),
        (
            r#"{"action":"run","command":"git status","tty":true}"#,
            run(CommandProfile::User, true),
        ),
        (
            r#"{"action":"run","command":"git status","tty":true,"profile":"clean"}"#,
            run(CommandProfile::Clean, true),
        ),
    ] {
        assert_eq!(request(arguments), expected, "{arguments}");
    }
    let named = |clean_start: &str, profile| {
        (
            request(&format!(
                r#"{{"action":"run","command":"git status","tty":true,"shell":{{"kind":"executable","path":"/opt/zsh"{clean_start}}}}}"#
            )),
            Some(CommandRequest::Run {
                command: "git status".to_owned(),
                cwd: std::env::temp_dir(),
                profile,
                shell: Some(PathBuf::from("/opt/zsh")),
                terminal: true,
            }),
        )
    };
    for (clean_start, profile) in [
        ("", CommandProfile::User),
        (r#","clean_start":false"#, CommandProfile::User),
        (r#","clean_start":true"#, CommandProfile::Clean),
    ] {
        let (decoded, expected) = named(clean_start, profile);
        assert_eq!(decoded, expected, "{clean_start}");
    }
    assert_eq!(
        request(r#"{"request":{"action":"interact","session_id":"shell-1"}}"#),
        Some(CommandRequest::Observe)
    );
    assert_eq!(
        request(r#"{"request":{"action":"interact","session_id":"shell-1","chars":"y"}}"#),
        Some(CommandRequest::SendInput {
            input: "y".to_owned()
        })
    );
    assert_eq!(
        request(r#"{"request":{"action":"stop","session_id":"shell-1"}}"#),
        Some(CommandRequest::Stop)
    );
    for invalid in [
        r#"{"request":{"action":"run","command":"git status","command":"rm -rf ."}}"#,
        r#"{"request":{"action":"run"}}"#,
        r#"{"action":"run","command":"git status","cwd":"missing-directory"}"#,
        "{",
    ] {
        assert_eq!(request(invalid), None, "{invalid}");
    }
}

#[test]
fn invalid_working_directories_fail_before_admission() {
    let description = description(r#"{"action":"run","command":"ls","cwd":"missing-directory"}"#);
    assert_eq!(description.effect, ToolEffect::None);
}

#[test]
fn calls_the_shell_cannot_validate_are_refused_before_admission() {
    let refusal = |arguments: &str| {
        let prepared = shell().prepare(arguments).unwrap();
        (
            prepared.describe().title,
            prepared.refusal().map(|output| output.content.clone()),
        )
    };
    for (arguments, title, content) in [
        (
            r#"{"request":{"action":"run"}}"#,
            "Running command",
            r#"{"error":{"code":"invalid_shell_request","executed":false,"problems":["request.command is required.","request.command is required."]}}"#,
        ),
        (
            r#"{"request":{"action":"stop"}}"#,
            "Stopping shell execution",
            r#"{"error":{"code":"invalid_shell_request","executed":false,"problems":["request.session_id is required."]}}"#,
        ),
        (
            r#"{"request":{"action":"run","command":"ls","cwd":"missing-directory"}}"#,
            "Running ls",
            "shell run cwd is invalid: FileNotFound",
        ),
    ] {
        assert_eq!(
            refusal(arguments),
            (title.to_owned(), Some(content.to_owned())),
            "{arguments}"
        );
    }
    for valid in [
        r#"{"request":{"action":"run","command":"ls"}}"#,
        r#"{"request":{"action":"stop","session_id":"shell-1"}}"#,
        r#"{"action":"run","command":"ls","cwd":"/nonexistent/oh-fx-missing-directory"}"#,
    ] {
        assert_eq!(refusal(valid).1, None, "{valid}");
    }
}

#[test]
fn shell_interaction_wait_bounds_empty_observations_without_delaying_writes() {
    for (has_input, requested, expected) in [
        (false, 0, 5_000),
        (false, 1_000, 5_000),
        (false, 5_000, 5_000),
        (false, 45_000, 45_000),
        (false, 300_000, 300_000),
        (true, 0, 0),
        (true, 1_000, 1_000),
        (true, 30_000, 30_000),
        (true, 300_000, 30_000),
    ] {
        assert_eq!(
            effective_interact_yield_time(has_input, requested),
            expected,
            "{has_input} {requested}"
        );
    }
}

#[test]
fn unresolved_working_directories_outside_the_workspace_reach_the_gate_and_fail_when_run() {
    let missing = "/nonexistent/oh-fx-missing-directory";
    let prepared = shell()
        .prepare(&format!(
            r#"{{"action":"run","command":"ls","cwd":"{missing}"}}"#
        ))
        .unwrap();
    assert_eq!(prepared.describe().effect, ToolEffect::Mutating);
    assert_eq!(
        prepared.command_request(),
        Some(&CommandRequest::Run {
            command: "ls".to_owned(),
            cwd: PathBuf::from(missing),
            profile: CommandProfile::User,
            shell: None,
            terminal: false,
        })
    );
    let output = execute(prepared, PathAccess::WorkspaceOrExternal);
    assert_eq!(output.status, ToolResultStatus::Failure);
    assert!(
        output.content.starts_with("shell run cwd is invalid: "),
        "{}",
        output.content
    );
    for escaping in ["../oh-fx-missing-directory", "~/oh-fx-missing-directory"] {
        let description = description(&format!(
            r#"{{"action":"run","command":"ls","cwd":"{escaping}"}}"#
        ));
        assert_eq!(description.effect, ToolEffect::Mutating, "{escaping}");
    }
}

#[test]
fn workspace_only_runs_fail_once_their_working_directory_leaves_the_workspace() {
    let directories = Directories::new();
    let sub = directories.workspace.join("sub");
    fs::create_dir(&sub).unwrap();
    let mut prepared = shell_in(&directories.workspace)
        .prepare(r#"{"action":"run","command":"ls","cwd":"sub"}"#)
        .unwrap();
    prepared.complete();
    fs::rename(&sub, directories.workspace.join("moved")).unwrap();
    symlink(&directories.outside, &sub).unwrap();
    let output = execute(prepared, PathAccess::WorkspaceOnly);
    assert_eq!(output.status, ToolResultStatus::Failure);
    assert_eq!(output.content, runtime_failure(PATH_OUTSIDE_WORKSPACE));
}

#[test]
fn approved_runs_fail_once_their_working_directory_is_replaced() {
    let replacements: [fn(&Directories); 3] = [
        |directories| {
            directories.move_build_away();
            symlink(&directories.outside, &directories.build).unwrap();
        },
        |directories| {
            directories.move_build_away();
            fs::create_dir(&directories.build).unwrap();
        },
        |directories| {
            let external = directories.build.parent().unwrap();
            fs::rename(external, external.with_file_name("moved")).unwrap();
            symlink(&directories.outside, external).unwrap();
            fs::create_dir(directories.outside.join("build")).unwrap();
        },
    ];
    for (index, replace) in replacements.into_iter().enumerate() {
        let directories = Directories::new();
        let prepared = directories.run_in_build();
        replace(&directories);
        let output = execute(prepared, PathAccess::WorkspaceOrExternal);
        assert_eq!(output.status, ToolResultStatus::Failure, "{index}");
        assert_eq!(
            output.content,
            runtime_failure(DIRECTORY_CHANGED),
            "{index}"
        );
    }
    let directories = Directories::new();
    let output = execute(directories.run_in_build(), PathAccess::WorkspaceOrExternal);
    assert!(output.content.contains(LAUNCHED), "{}", output.content);
}

#[test]
fn working_directories_are_pinned_again_when_the_call_completes() {
    let directories = Directories::new();
    let arguments =
        serde_json::json!({"action": "run", "command": "ls", "cwd": directories.build}).to_string();
    let shell = shell_in(&directories.workspace);
    let stale = shell.prepare(&arguments).unwrap();
    let mut completed = shell.prepare(&arguments).unwrap();
    directories.move_build_away();
    fs::create_dir(&directories.build).unwrap();
    completed.complete();
    assert_eq!(
        execute(stale, PathAccess::WorkspaceOrExternal).content,
        runtime_failure(DIRECTORY_CHANGED)
    );
    let output = execute(completed, PathAccess::WorkspaceOrExternal);
    assert!(output.content.contains(LAUNCHED), "{}", output.content);
}

#[test]
fn working_directories_that_are_not_directories_fail_before_launch() {
    let directories = Directories::new();
    let file = directories.workspace.join("file");
    fs::write(&file, "").unwrap();
    let mut prepared = shell_in(&directories.workspace)
        .prepare(r#"{"action":"run","command":"ls","cwd":"file"}"#)
        .unwrap();
    prepared.complete();
    let output = execute(prepared, PathAccess::WorkspaceOnly);
    assert_eq!(output.content, runtime_failure("NotDir"));
}

#[test]
fn only_resolved_run_directories_are_applicable_targets_and_they_resolve_again() {
    let workspace = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(workspace.path()).unwrap();
    fs::create_dir(root.join("sub")).unwrap();
    let shell = shell_in(&root);
    let target = |arguments: &str| shell.prepare(arguments).unwrap().applicable_target();
    let directory = |path: PathBuf| {
        Some(ApplicableTarget {
            path,
            kind: TargetKind::Directory,
        })
    };
    assert_eq!(
        target(r#"{"action":"run","command":"ls"}"#),
        directory(root.clone())
    );
    assert_eq!(
        target(r#"{"action":"run","command":"ls","cwd":"sub"}"#),
        directory(root.join("sub"))
    );
    for arguments in [
        r#"{"action":"run","command":"ls","cwd":"missing"}"#,
        r#"{"action":"run","command":"ls","cwd":"/nonexistent/oh-fx-missing-directory"}"#,
        r#"{"action":"interact","session_id":"s1"}"#,
        r#"{"action":"stop","session_id":"s1"}"#,
        r#"{"action":"run"}"#,
    ] {
        assert_eq!(target(arguments), None, "{arguments}");
    }
    let prepared = shell
        .prepare(r#"{"action":"run","command":"ls","cwd":"sub"}"#)
        .unwrap();
    fs::remove_dir(root.join("sub")).unwrap();
    assert_eq!(prepared.applicable_target(), None);
}
