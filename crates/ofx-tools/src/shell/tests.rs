use ofx_contract::{Tool, ToolActivity, ToolCallId, ToolEffect, ToolResultStatus};
use ofx_exec::SessionSupervisor;

use super::*;

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}

fn shell() -> Shell {
    Shell::new(
        std::env::temp_dir(),
        ManagedExecutions::new(SessionSupervisor::new("/nonexistent")),
        None,
    )
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
    let run = |command: &str, terminal| {
        Some(CommandRequest::Run {
            command: command.to_owned(),
            cwd: std::env::temp_dir(),
            terminal,
        })
    };
    assert_eq!(
        request(r#"{"request":{"action":"run","command":"git status","cwd":"."}}"#),
        run("git status", false)
    );
    assert_eq!(
        request(r#"{"action":"run","command":"git status","tty":true}"#),
        run("git status", true)
    );
    assert_eq!(
        request(r#"{"request":{"action":"interact","session_id":"shell-1"}}"#),
        Some(CommandRequest::Observe)
    );
    assert_eq!(
        request(r#"{"request":{"action":"interact","session_id":"shell-1","chars":"y"}}"#),
        Some(CommandRequest::SendInput)
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
            terminal: false,
        })
    );
    let output = block_on(prepared.execute(ToolContext::new(
        ToolCallId::new("call-1"),
        CancellationToken::new(),
        PathAccess::WorkspaceOrExternal,
    )));
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
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(workspace.path()).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
    let shell = Shell::new(
        &root,
        ManagedExecutions::new(SessionSupervisor::new("/nonexistent")),
        None,
    );
    let request = request::decode(r#"{"action":"run","command":"ls","cwd":"link"}"#).unwrap();
    let output = block_on(shell.context.run(
        request,
        root.clone(),
        None,
        PathAccess::WorkspaceOnly,
        &CancellationToken::new(),
    ));
    assert_eq!(output.status, ToolResultStatus::Failure);
    assert_eq!(output.content, runtime_failure(PATH_OUTSIDE_WORKSPACE));
}
