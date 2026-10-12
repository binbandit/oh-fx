use std::env;
use std::ffi::OsString;
use std::fs;
use std::panic;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ofx_contract::{
    BoxFuture, PathAccess, Tool, ToolCallId, ToolContext, ToolOutput, ToolResultStatus,
};
use ofx_exec::{
    Lifecycle, ManagedExecutions, ReturnCondition, SessionSupervisor, ShellSpec, StartRequest,
    StructuredErrorCode, TerminalStore, Terminals, is_terminal_control_invocation,
    is_terminal_launcher_invocation, run_terminal_control, run_terminal_launcher,
};
use ofx_tools::{Shell, ShellTerminals, TerminalOwner};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const OWNER: &str = "owner-session";
const BASH: &str = "/bin/bash";
const LONG: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(10);
const STORE_VARIABLE: &str = "OH_FX_TEST_TERMINAL_STORE";

const TESTS: &[(&str, fn())] = &[
    (
        "a_terminal_run_reports_its_output_and_status",
        a_terminal_run_reports_its_output_and_status,
    ),
    (
        "a_running_terminal_takes_input_and_reports_what_it_accepted",
        a_running_terminal_takes_input_and_reports_what_it_accepted,
    ),
    (
        "stopping_a_terminal_ends_its_command",
        stopping_a_terminal_ends_its_command,
    ),
    (
        "a_terminal_of_another_owner_answers_its_failure",
        a_terminal_of_another_owner_answers_its_failure,
    ),
    (
        "a_stop_after_the_command_exited_reports_its_status",
        a_stop_after_the_command_exited_reports_its_status,
    ),
    (
        "a_preempted_interact_leaves_its_output_for_the_stop",
        a_preempted_interact_leaves_its_output_for_the_stop,
    ),
    (
        "a_terminal_of_an_exited_process_has_ended",
        a_terminal_of_an_exited_process_has_ended,
    ),
];

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if is_terminal_control_invocation(&args) {
        run_terminal_control(&args);
    }
    if is_terminal_launcher_invocation(&args) {
        run_terminal_launcher();
    }
    if let Some(store) = env::var_os(STORE_VARIABLE) {
        return ended_with_its_owner(PathBuf::from(store));
    }
    let filters: Vec<String> = args
        .iter()
        .filter_map(|argument| argument.to_str())
        .filter(|argument| !argument.starts_with('-'))
        .map(str::to_owned)
        .collect();
    let mut failed = Vec::new();
    for (name, test) in TESTS {
        if !filters.is_empty() && !filters.iter().any(|filter| name.contains(filter.as_str())) {
            continue;
        }
        let outcome = panic::catch_unwind(test);
        println!(
            "test {name} ... {}",
            if outcome.is_ok() { "ok" } else { "FAILED" }
        );
        if outcome.is_err() {
            failed.push(name);
        }
    }
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        println!("failed: {failed:?}");
        ExitCode::FAILURE
    }
}

struct Fixture {
    runtime: tokio::runtime::Runtime,
    terminals: Terminals,
    executions: ManagedExecutions,
    owner: TerminalOwner,
    shell: Shell,
    store: PathBuf,
    workspace: PathBuf,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create a directory");
        let root = fs::canonicalize(directory.path()).expect("canonicalize the directory");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("create the workspace");
        let store = root.join("store");
        let terminals = Terminals::new(TerminalStore::new(&store).expect("open the store"));
        let owner = TerminalOwner::default();
        owner.bind(Some(OWNER.to_owned()));
        let executions = ManagedExecutions::new(SessionSupervisor::new(
            env::current_exe().expect("the test executable"),
        ));
        let shell = Shell::with_terminals(
            workspace.clone(),
            executions.clone(),
            None,
            ShellTerminals {
                terminals: terminals.clone(),
                owner: owner.clone(),
                always_offered: false,
            },
        );
        Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build a runtime"),
            terminals,
            executions,
            owner,
            shell,
            store,
            workspace,
            _directory: directory,
        }
    }

    fn future(&self, request: &Value) -> BoxFuture<'static, ToolOutput> {
        let mut prepared = self
            .shell
            .prepare(&json!({ "request": request }).to_string())
            .expect("prepare the call");
        prepared.complete();
        prepared.execute(ToolContext::new(
            ToolCallId::new("call-1"),
            CancellationToken::new(),
            PathAccess::WorkspaceOrExternal,
        ))
    }

    fn call(&self, request: &Value) -> (ToolResultStatus, Value) {
        parsed(&self.runtime.block_on(self.future(request)))
    }

    fn created(&self, name: &str) {
        let deadline = Instant::now() + LONG;
        while !self.workspace.join(name).exists() {
            assert!(Instant::now() < deadline, "{name} was never created");
            thread::sleep(POLL);
        }
    }

    fn lifecycle(&self, id: &str) -> Option<Lifecycle> {
        self.terminals
            .list(OWNER)
            .expect("list the terminals")
            .into_iter()
            .find(|facts| facts.session_id == id)
            .map(|facts| facts.lifecycle)
    }

    fn reaches(&self, id: &str, wanted: Lifecycle) {
        let deadline = Instant::now() + LONG;
        while self.lifecycle(id) != Some(wanted) {
            assert!(
                Instant::now() < deadline,
                "the terminal never became {wanted:?}"
            );
            thread::sleep(POLL);
        }
    }

    fn command_started(&self, id: &str) {
        let deadline = Instant::now() + LONG;
        while self
            .terminals
            .inspect(OWNER, id)
            .expect("inspect the terminal")
            .session
            .lifecycle
            == Lifecycle::Starting
        {
            assert!(Instant::now() < deadline, "the command never started");
            thread::sleep(POLL);
        }
    }

    fn run(&self, command: &str, yield_time_ms: u32) -> (ToolResultStatus, Value) {
        self.call(&json!({
            "action": "run",
            "command": command,
            "tty": true,
            "shell": {"kind": "executable", "path": BASH, "clean_start": true},
            "yield_time_ms": yield_time_ms,
        }))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.runtime
            .block_on(self.terminals.close_owned_terminals());
    }
}

fn parsed(output: &ToolOutput) -> (ToolResultStatus, Value) {
    let body = serde_json::from_str(&output.content).expect("a JSON result");
    (output.status, body)
}

fn session_id(body: &Value) -> String {
    body["session_id"]
        .as_str()
        .expect("a retained session")
        .to_owned()
}

fn a_terminal_run_reports_its_output_and_status() {
    let fixture = Fixture::new();
    let (status, body) = fixture.run("printf 'hello from tty\\n'; exit 3", 30_000);
    assert_eq!(status, ToolResultStatus::Failure, "{body}");
    assert_eq!(body["state"], "completed", "{body}");
    assert_eq!(body["backend"], "tty");
    assert_eq!(body["persistence"], "session");
    assert_eq!(body["exit_code"], 3);
    assert_eq!(body["session_id"], Value::Null);
    assert!(
        body["output_delta"]
            .as_str()
            .is_some_and(|output| output.contains("hello from tty")),
        "{body}"
    );
    assert!(fixture.shell.spec().input_schema.contains("\"tty\""));
    let listed: Vec<Lifecycle> = fixture
        .terminals
        .list(OWNER)
        .expect("list the terminals")
        .into_iter()
        .map(|facts| facts.lifecycle)
        .collect();
    assert_eq!(listed, [Lifecycle::Closed]);
}

fn a_running_terminal_takes_input_and_reports_what_it_accepted() {
    let fixture = Fixture::new();
    let (status, started) = fixture.run("read -r line; echo \"got $line\"", 0);
    assert_eq!(status, ToolResultStatus::Success, "{started}");
    assert_eq!(started["state"], "running", "{started}");
    let id = session_id(&started);
    let (status, answered) = fixture.call(&json!({
        "action": "interact",
        "session_id": id,
        "chars": "hi\n",
        "yield_time_ms": 20_000,
    }));
    assert_eq!(status, ToolResultStatus::Success, "{answered}");
    assert_eq!(answered["accepted_bytes"], 3, "{answered}");
    assert_eq!(answered["state"], "completed", "{answered}");
    assert_eq!(answered["exit_code"], 0, "{answered}");
    assert!(
        answered["output_delta"]
            .as_str()
            .is_some_and(|output| output.contains("got hi")),
        "{answered}"
    );
    assert_eq!(fixture.lifecycle(&id), Some(Lifecycle::Closed));
    let (_, again) = fixture.call(&json!({"action": "interact", "session_id": id}));
    assert_eq!(again["state"], "completed", "{again}");
    assert_eq!(again["output_delta"], "", "{again}");
    let (status, late) = fixture.call(&json!({
        "action": "interact",
        "session_id": id,
        "chars": "late\n",
    }));
    assert_eq!(status, ToolResultStatus::Failure);
    assert_eq!(late["error"]["code"], "ExecutionTerminal", "{late}");
    let (_, waiting) = fixture.run("read -r line; sleep 30", 0);
    let waiting = session_id(&waiting);
    let (status, accepted) = fixture.call(&json!({
        "action": "interact",
        "session_id": waiting,
        "chars": "abc\n",
        "yield_time_ms": 0,
    }));
    assert_eq!(status, ToolResultStatus::Success, "{accepted}");
    assert_eq!(accepted["accepted_bytes"], 4, "{accepted}");
    assert_eq!(accepted["state"], "running", "{accepted}");
}

fn a_stop_after_the_command_exited_reports_its_status() {
    let fixture = Fixture::new();
    let (_, started) = fixture.run("printf 'done\\n'; exit 4", 0);
    let id = session_id(&started);
    fixture.reaches(&id, Lifecycle::Exited);
    let (status, stopped) = fixture.call(&json!({"action": "stop", "session_id": id}));
    assert_eq!(status, ToolResultStatus::Success, "{stopped}");
    assert_eq!(stopped["state"], "completed", "{stopped}");
    assert_eq!(stopped["exit_code"], 4, "{stopped}");
    assert_eq!(fixture.lifecycle(&id), Some(Lifecycle::Closed));
}

fn a_preempted_interact_leaves_its_output_for_the_stop() {
    let fixture = Fixture::new();
    let (_, started) = fixture.run(
        "set +m; printf 'early\\n'; sleep 30 & echo $! > sleeper.tmp && mv sleeper.tmp sleeper; \
         wait; printf late-output",
        0,
    );
    let id = session_id(&started);
    fixture.created("sleeper");
    let interact = fixture.future(&json!({
        "action": "interact",
        "session_id": id,
        "yield_time_ms": 20_000,
    }));
    let stop = fixture.future(&json!({"action": "stop", "session_id": id}));
    let (interacted, stopped) = fixture.runtime.block_on(async {
        let interacting = tokio::spawn(interact);
        while let Ok(waiter) = fixture.executions.reserve_external_wait(&id) {
            fixture.executions.release_external_wait(&id, waiter);
            tokio::task::yield_now().await;
        }
        let stopped = stop.await;
        (interacting.await.expect("the interact"), stopped)
    });
    let (_, interacted) = parsed(&interacted);
    let (status, stopped) = parsed(&stopped);
    assert_eq!(interacted["error"]["code"], "WaitPreempted", "{interacted}");
    assert_eq!(status, ToolResultStatus::Success, "{stopped}");
    assert_eq!(stopped["state"], "stopped", "{stopped}");
    let output = stopped["output_delta"].as_str().unwrap_or_default();
    assert!(output.contains("early"), "{stopped}");
    assert!(output.contains("late-output"), "{stopped}");
}

fn a_terminal_of_an_exited_process_has_ended() {
    let fixture = Fixture::new();
    let reported = Command::new(env::current_exe().expect("the test executable"))
        .env(STORE_VARIABLE, &fixture.store)
        .current_dir(&fixture.workspace)
        .stdin(Stdio::null())
        .output()
        .expect("run the scenario");
    let id = String::from_utf8_lossy(&reported.stdout).trim().to_owned();
    assert!(id.starts_with("shell-"), "{id:?}");
    for action in ["interact", "stop"] {
        let (status, ended) = fixture.call(&json!({"action": action, "session_id": id}));
        assert_eq!(status, ToolResultStatus::Failure);
        assert_eq!(
            ended,
            json!({"error": {
                "tool": "shell",
                "code": "TerminalEnded",
                "session_id": id,
                "message": "This terminal ended when the oh-fx process that started it exited. It cannot be resumed. Start a new terminal with shell action run and tty true.",
                "retryable": false,
            }}),
            "{action}"
        );
    }
}

fn ended_with_its_owner(store: PathBuf) -> ExitCode {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a runtime");
    let terminals = Terminals::new(TerminalStore::new(store).expect("open the store"));
    let started = runtime
        .block_on(terminals.start(
            &StartRequest {
                owner_session_id: OWNER,
                cwd: &env::current_dir().expect("the scenario directory"),
                command: "set +m; sleep 30",
                shell: &ShellSpec::Executable {
                    path: BASH.into(),
                    clean_start: true,
                },
                return_when: ReturnCondition::Started,
                wait_ceiling: LONG,
                timeout_ms: None,
            },
            &CancellationToken::new(),
        ))
        .expect("start a terminal");
    runtime.block_on(terminals.close_owned_terminals());
    println!("{}", started.session.session_id);
    ExitCode::SUCCESS
}

fn stopping_a_terminal_ends_its_command() {
    let fixture = Fixture::new();
    for force in [false, true] {
        let (_, started) = fixture.run("set +m; sleep 30", 0);
        let id = session_id(&started);
        fixture.command_started(&id);
        let (status, stopped) =
            fixture.call(&json!({"action": "stop", "session_id": id, "force": force}));
        assert_eq!(status, ToolResultStatus::Success, "{stopped}");
        assert_eq!(stopped["state"], "stopped", "{stopped}");
        assert_eq!(stopped["backend"], "tty");
        assert_eq!(
            fixture
                .terminals
                .inspect(OWNER, &id)
                .map_err(|failure| failure.code),
            Err(StructuredErrorCode::AuthorityDenied),
            "the stop left the terminal open"
        );
        let (_, retained) = fixture.call(&json!({"action": "stop", "session_id": id}));
        assert_eq!(retained["state"], "stopped", "{retained}");
    }
}

fn a_terminal_of_another_owner_answers_its_failure() {
    let fixture = Fixture::new();
    let (_, started) = fixture.run("sleep 30", 0);
    let id = session_id(&started);
    fixture.owner.bind(Some("another-session".to_owned()));
    let (status, denied) = fixture.call(&json!({
        "action": "interact",
        "session_id": id,
        "chars": "x",
    }));
    assert_eq!(status, ToolResultStatus::Failure);
    assert_eq!(
        denied,
        json!({"failure": {
            "action": "wait",
            "code": "authority_denied",
            "session_id": id,
            "retryable": false,
        }})
    );
    fixture.owner.bind(None);
    for request in [
        json!({"action": "interact", "session_id": id, "chars": "x"}),
        json!({"action": "stop", "session_id": id}),
    ] {
        let (status, unavailable) = fixture.call(&request);
        assert_eq!(status, ToolResultStatus::Failure);
        assert_eq!(
            unavailable["error"]["code"], "TerminalAuthorityUnavailable",
            "{request} {unavailable}"
        );
    }
    let (status, unbound) = fixture.run("true", 0);
    assert_eq!(status, ToolResultStatus::Failure);
    assert_eq!(unbound["error"]["code"], "unavailable", "{unbound}");
    assert!(!fixture.shell.spec().input_schema.contains("\"tty\""));
}
