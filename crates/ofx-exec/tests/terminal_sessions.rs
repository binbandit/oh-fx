use std::env;
use std::ffi::OsString;
use std::fs;
use std::panic;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant};

use ofx_exec::{
    Action, Lifecycle, ReturnCondition, ReturnOutcome, SessionFacts, ShellSpec, StartRequest,
    StructuredErrorCode, TerminalFailure, TerminalRecord, TerminalStore, Terminals, Termination,
    is_terminal_control_invocation, is_terminal_launcher_invocation, run_terminal_control,
    run_terminal_launcher,
};
use rustix::process::{Pid, Signal, kill_process};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

const BASH: &str = "/bin/bash";
const OWNER: &str = "owner-session";
const STRANGER: &str = "another-session";
const LONG: Duration = Duration::from_secs(20);
const SHORT: Duration = Duration::from_millis(50);
const POLL: Duration = Duration::from_millis(10);
const WAIT_FOR_GO: &str = "while [ ! -e go ]; do sleep 0.05; done; exit 4";
const SIGKILL: u32 = 9;

const TESTS: &[(&str, fn())] = &[
    (
        "a_command_exits_with_its_status_in_its_terminal",
        a_command_exits_with_its_status_in_its_terminal,
    ),
    ("a_signal_ends_the_command", a_signal_ends_the_command),
    (
        "a_started_terminal_reports_its_exit_to_a_later_wait",
        a_started_terminal_reports_its_exit_to_a_later_wait,
    ),
    (
        "ending_the_owner_ends_its_terminals_and_their_jobs",
        ending_the_owner_ends_its_terminals_and_their_jobs,
    ),
    (
        "a_missing_shell_is_unavailable",
        a_missing_shell_is_unavailable,
    ),
    (
        "terminals_answer_only_their_owner",
        terminals_answer_only_their_owner,
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
    runtime: Runtime,
    terminals: Terminals,
    cwd: PathBuf,
    store: PathBuf,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create a directory");
        let cwd = fs::canonicalize(directory.path()).expect("canonicalize the directory");
        let store = cwd.join("store");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build a runtime");
        let terminals = Terminals::new(TerminalStore::new(&store).expect("open the store"));
        Self {
            runtime,
            terminals,
            cwd,
            store,
            _directory: directory,
        }
    }

    fn start(&self, command: &str, return_when: ReturnCondition) -> Start {
        self.start_in(command, return_when, &bash())
            .expect("start a terminal")
    }

    fn start_in(
        &self,
        command: &str,
        return_when: ReturnCondition,
        shell: &ShellSpec,
    ) -> Result<Start, TerminalFailure> {
        self.runtime
            .block_on(self.terminals.start(
                &StartRequest {
                    owner_session_id: OWNER,
                    cwd: &self.cwd,
                    command,
                    shell,
                    return_when,
                    wait_ceiling: LONG,
                },
                &CancellationToken::new(),
            ))
            .map(|outcome| Start {
                id: outcome.session.session_id.clone(),
                facts: outcome.session,
                outcome: outcome.outcome,
            })
    }

    fn wait(
        &self,
        owner: &str,
        id: &str,
        condition: ReturnCondition,
        ceiling: Duration,
        cancel: &CancellationToken,
    ) -> Result<ReturnOutcome, TerminalFailure> {
        self.runtime
            .block_on(self.terminals.wait(owner, id, condition, ceiling, cancel))
            .map(|outcome| outcome.outcome)
    }

    fn wait_exit(&self, id: &str) -> Result<ReturnOutcome, TerminalFailure> {
        self.wait(
            OWNER,
            id,
            ReturnCondition::Exit,
            LONG,
            &CancellationToken::new(),
        )
    }

    fn end_owner(&self) {
        self.runtime
            .block_on(self.terminals.close_owned_terminals());
    }

    fn file_until(&self, name: &str, done: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + LONG;
        loop {
            let text = fs::read_to_string(self.cwd.join(name)).unwrap_or_default();
            if done(&text) {
                return text;
            }
            assert!(Instant::now() < deadline, "{name} holds {text:?}");
            thread::sleep(POLL);
        }
    }

    fn records(&self) -> Vec<TerminalRecord> {
        TerminalStore::new(&self.store)
            .expect("open the store again")
            .reconcile_owner(OWNER, 0)
            .expect("read the saved records")
    }

    fn record(&self, id: &str) -> (Lifecycle, Option<Termination>) {
        self.records()
            .into_iter()
            .find(|record| record.session_id == id)
            .map(|record| (record.lifecycle, record.termination))
            .expect("the terminal's saved record")
    }

    fn lifecycle(&self, id: &str) -> Lifecycle {
        self.terminals
            .inspect(OWNER, id)
            .expect("inspect the terminal")
            .session
            .lifecycle
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.end_owner();
    }
}

struct Start {
    id: String,
    facts: SessionFacts,
    outcome: ReturnOutcome,
}

fn bash() -> ShellSpec {
    ShellSpec::Executable {
        path: PathBuf::from(BASH),
        clean_start: true,
    }
}

fn failure_code(failure: Result<impl Sized, TerminalFailure>) -> StructuredErrorCode {
    match failure {
        Ok(_) => panic!("the action succeeded"),
        Err(failure) => failure.code,
    }
}

fn alive(pid: Pid) -> bool {
    let pid = pid.as_raw_nonzero().to_string();
    let state = if cfg!(target_os = "linux") {
        fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .map(|(_, fields)| fields.trim_start().to_owned())
            })
            .unwrap_or_default()
    } else {
        Command::new("ps")
            .args(["-o", "stat=", "-p", &pid])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    !state.is_empty() && !state.starts_with('Z')
}

fn gone_in_time(pid: Pid) -> bool {
    let deadline = Instant::now() + LONG;
    while alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL);
    }
    true
}

fn a_command_exits_with_its_status_in_its_terminal() {
    let fixture = Fixture::new();
    let started = fixture.start(
        &format!(
            "[ \"$(stty size)\" = '24 80' ] || exit 1; [ \"$PWD\" = '{}' ] || exit 2; exit 7",
            fixture.cwd.display()
        ),
        ReturnCondition::Exit,
    );
    assert_eq!(started.outcome, ReturnOutcome::Exited(7));
    assert_eq!(started.facts.lifecycle, Lifecycle::Exited);
    assert!(!started.facts.timed_out);
    assert!(started.id.starts_with("shell-"), "{}", started.id);
    assert_eq!(fixture.wait_exit(&started.id), Ok(ReturnOutcome::Exited(7)));
    assert_eq!(
        fixture.record(&started.id),
        (Lifecycle::Exited, Some(Termination::Exited(7)))
    );
}

fn a_signal_ends_the_command() {
    let fixture = Fixture::new();
    let started = fixture.start("kill -KILL $$", ReturnCondition::Exit);
    assert_eq!(started.outcome, ReturnOutcome::Signal(SIGKILL));
    assert_eq!(started.facts.lifecycle, Lifecycle::Exited);
    assert_eq!(
        fixture.record(&started.id),
        (Lifecycle::Exited, Some(Termination::Signal(SIGKILL)))
    );
}

fn a_started_terminal_reports_its_exit_to_a_later_wait() {
    let fixture = Fixture::new();
    let started = fixture.start(WAIT_FOR_GO, ReturnCondition::Started);
    assert_eq!(started.outcome, ReturnOutcome::Started);
    assert_eq!(started.facts.lifecycle, Lifecycle::Running);
    assert_eq!(
        fixture.wait(
            OWNER,
            &started.id,
            ReturnCondition::Started,
            LONG,
            &CancellationToken::new()
        ),
        Ok(ReturnOutcome::Started)
    );
    assert_eq!(
        fixture.wait(
            OWNER,
            &started.id,
            ReturnCondition::Exit,
            SHORT,
            &CancellationToken::new()
        ),
        Ok(ReturnOutcome::SafetyCeiling)
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        fixture.wait(
            OWNER,
            &started.id,
            ReturnCondition::Exit,
            Duration::MAX,
            &cancelled
        ),
        Ok(ReturnOutcome::Cancelled)
    );
    assert_eq!(fixture.lifecycle(&started.id), Lifecycle::Running);
    fs::write(fixture.cwd.join("go"), "").expect("let the command finish");
    assert_eq!(fixture.wait_exit(&started.id), Ok(ReturnOutcome::Exited(4)));
    assert_eq!(fixture.lifecycle(&started.id), Lifecycle::Exited);
}

fn ending_the_owner_ends_its_terminals_and_their_jobs() {
    let fixture = Fixture::new();
    let started = fixture.start(
        "set +m; sleep 30 & echo $! > child.tmp && mv child.tmp child; wait",
        ReturnCondition::Started,
    );
    let child = fixture
        .file_until("child", |text| text.ends_with('\n'))
        .trim()
        .parse()
        .ok()
        .and_then(Pid::from_raw)
        .expect("the background child's pid");
    assert!(alive(child));
    fixture.end_owner();
    let killed = gone_in_time(child);
    if !killed {
        let _ = kill_process(child, Signal::KILL);
    }
    assert!(killed, "the background child outlived its owner");
    assert_eq!(fixture.lifecycle(&started.id), Lifecycle::Lost);
    assert_eq!(fixture.record(&started.id), (Lifecycle::Lost, None));
    assert_eq!(
        failure_code(fixture.start_in("true", ReturnCondition::Exit, &bash())),
        StructuredErrorCode::Cancelled
    );
    assert_eq!(fixture.records().len(), 1);
}

fn a_missing_shell_is_unavailable() {
    let fixture = Fixture::new();
    let failure = fixture
        .start_in(
            "true",
            ReturnCondition::Exit,
            &ShellSpec::Executable {
                path: PathBuf::from("/nonexistent/bash"),
                clean_start: true,
            },
        )
        .err()
        .expect("the start fails");
    assert_eq!(failure.action, Action::Start);
    assert_eq!(failure.code, StructuredErrorCode::ShellUnavailable);
    let id = failure.session_id.expect("the failed terminal's id");
    assert_eq!(fixture.lifecycle(&id), Lifecycle::Lost);
    assert_eq!(fixture.record(&id), (Lifecycle::Lost, None));
    assert_eq!(
        failure_code(fixture.start_in(
            "true",
            ReturnCondition::Exit,
            &ShellSpec::Executable {
                path: PathBuf::from("/bin/fish"),
                clean_start: true,
            },
        )),
        StructuredErrorCode::InvalidRequest
    );
}

fn terminals_answer_only_their_owner() {
    let fixture = Fixture::new();
    let started = fixture.start("exit 0", ReturnCondition::Exit);
    assert_eq!(
        failure_code(fixture.terminals.inspect(STRANGER, &started.id)),
        StructuredErrorCode::AuthorityDenied
    );
    assert_eq!(
        failure_code(fixture.wait(
            STRANGER,
            &started.id,
            ReturnCondition::Exit,
            LONG,
            &CancellationToken::new()
        )),
        StructuredErrorCode::AuthorityDenied
    );
    assert_eq!(
        failure_code(fixture.wait_exit("shell-AAAAAAAAAAAAAAAAAAAAAA")),
        StructuredErrorCode::SessionNotFound
    );
    let inspected = fixture
        .terminals
        .inspect(OWNER, &started.id)
        .expect("inspect the terminal");
    assert_eq!(inspected.shell, BASH);
    assert_eq!(inspected.cwd, fixture.cwd.to_string_lossy());
    assert_eq!(inspected.command.as_deref(), Some("exit 0"));
    assert_eq!(inspected.session, started.facts);
}
