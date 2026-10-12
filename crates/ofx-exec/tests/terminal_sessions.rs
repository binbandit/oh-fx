use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ofx_exec::{
    Action, ClosePolicy, Lifecycle, ReturnCondition, ReturnOutcome, SessionFacts, ShellSpec,
    StartRequest, StructuredErrorCode, TerminalFailure, TerminalSignal, TerminalStore, Terminals,
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
const STALLED: Duration = Duration::from_millis(500);
const WAIT_FOR_GO: &str = "while [ ! -e go ]; do sleep 0.05; done; exit 4";
const GRACEFUL_CLOSE: Duration = Duration::from_millis(800);
const PROMPT_CLOSE: Duration = Duration::from_secs(5);
const SCENARIO_VARIABLE: &str = "OH_FX_TEST_TERMINAL_SCENARIO";
const STORE_VARIABLE: &str = "OH_FX_TEST_TERMINAL_STORE";
const SOCKET_MODE_VARIABLE: &str = "OH_FX_TEST_TERMINAL_SOCKET_MODES";
const NEVER_PREPARES_VARIABLE: &str = "OH_FX_TEST_TERMINAL_NEVER_PREPARES";
const SIGHUP: u32 = 1;
const SIGTERM: i32 = 15;
const SIGKILL: u32 = 9;

const TESTS: &[(&str, fn())] = &[
    (
        "a_command_exits_with_its_status_in_its_terminal",
        a_command_exits_with_its_status_in_its_terminal,
    ),
    ("a_signal_ends_the_command", a_signal_ends_the_command),
    (
        "output_is_read_once_and_rendered_on_the_screen",
        output_is_read_once_and_rendered_on_the_screen,
    ),
    (
        "a_started_terminal_takes_input_until_it_exits",
        a_started_terminal_takes_input_until_it_exits,
    ),
    (
        "a_timeout_kills_the_command_and_marks_it_timed_out",
        a_timeout_kills_the_command_and_marks_it_timed_out,
    ),
    (
        "a_graceful_close_kills_a_command_that_ignores_terminate",
        a_graceful_close_kills_a_command_that_ignores_terminate,
    ),
    (
        "a_timeout_during_a_graceful_close_ends_it_early",
        a_timeout_during_a_graceful_close_ends_it_early,
    ),
    (
        "a_pending_wait_ends_when_its_terminal_closes",
        a_pending_wait_ends_when_its_terminal_closes,
    ),
    (
        "a_forced_close_ends_the_command_and_is_recorded",
        a_forced_close_ends_the_command_and_is_recorded,
    ),
    (
        "a_forced_close_ends_a_terminal_whose_input_is_full",
        a_forced_close_ends_a_terminal_whose_input_is_full,
    ),
    (
        "a_terminal_closed_while_input_waits_reads_only_what_it_accepted",
        a_terminal_closed_while_input_waits_reads_only_what_it_accepted,
    ),
    (
        "the_screen_is_unavailable_after_output_it_cannot_parse",
        the_screen_is_unavailable_after_output_it_cannot_parse,
    ),
    (
        "requests_are_checked_before_anything_runs",
        requests_are_checked_before_anything_runs,
    ),
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
    (
        "saved_terminals_answer_after_their_process_ended",
        saved_terminals_answer_after_their_process_ended,
    ),
    (
        "the_command_is_the_foreground_job_of_its_terminal",
        the_command_is_the_foreground_job_of_its_terminal,
    ),
    (
        "a_shell_that_exits_during_startup_fails_the_start",
        a_shell_that_exits_during_startup_fails_the_start,
    ),
    (
        "the_control_socket_is_private_to_its_owner",
        the_control_socket_is_private_to_its_owner,
    ),
    (
        "a_launcher_that_never_prepares_fails_the_start",
        a_launcher_that_never_prepares_fails_the_start,
    ),
    (
        "a_forced_close_ends_a_terminal_that_never_started",
        a_forced_close_ends_a_terminal_that_never_started,
    ),
    (
        "a_crashed_owner_leaves_no_terminal_behind",
        a_crashed_owner_leaves_no_terminal_behind,
    ),
    (
        "signals_reach_the_command_and_its_process_group",
        signals_reach_the_command_and_its_process_group,
    ),
    (
        "signals_need_a_live_terminal_of_their_owner",
        signals_need_a_live_terminal_of_their_owner,
    ),
    (
        "closes_reach_processes_that_left_the_process_group",
        closes_reach_processes_that_left_the_process_group,
    ),
    (
        "a_forced_close_ends_the_group_holding_a_terminal_whose_shell_exited",
        a_forced_close_ends_the_group_holding_a_terminal_whose_shell_exited,
    ),
    (
        "a_close_ends_promptly_while_another_group_holds_the_terminal",
        a_close_ends_promptly_while_another_group_holds_the_terminal,
    ),
];

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if is_terminal_control_invocation(&args) {
        record_control_socket_mode(&args);
        run_terminal_control(&args);
    }
    if is_terminal_launcher_invocation(&args) {
        if env::var_os(NEVER_PREPARES_VARIABLE).is_some() {
            return consume_launcher_config();
        }
        run_terminal_launcher();
    }
    if let Some(scenario) = env::var_os(SCENARIO_VARIABLE) {
        return run_scenario(&scenario);
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
    _directory: Option<tempfile::TempDir>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create a directory");
        let cwd = fs::canonicalize(directory.path()).expect("canonicalize the directory");
        Self::at(Some(directory), cwd.join("store"), cwd)
    }

    fn at(directory: Option<tempfile::TempDir>, store: PathBuf, cwd: PathBuf) -> Self {
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
        self.start_with(command, return_when, shell, None)
    }

    fn start_with(
        &self,
        command: &str,
        return_when: ReturnCondition,
        shell: &ShellSpec,
        timeout_ms: Option<u64>,
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
                    timeout_ms,
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

    fn close(&self, id: &str, policy: ClosePolicy) -> Result<SessionFacts, TerminalFailure> {
        self.runtime
            .block_on(self.terminals.close(OWNER, id, policy))
            .map(|closed| closed.session)
    }

    fn read_text(&self, id: &str) -> String {
        let read = self.terminals.read(OWNER, id).expect("read the terminal");
        String::from_utf8_lossy(&read.output).into_owned()
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

    fn records(&self) -> Terminals {
        Terminals::new(TerminalStore::new(&self.store).expect("open the store again"))
    }

    fn record(&self, id: &str) -> (Lifecycle, Option<ReturnOutcome>) {
        let saved = self.records();
        let lifecycle = saved
            .list(OWNER)
            .expect("list the saved records")
            .into_iter()
            .find(|facts| facts.session_id == id)
            .map(|facts| facts.lifecycle)
            .expect("the terminal's saved record");
        let outcome = self
            .runtime
            .block_on(saved.wait(
                OWNER,
                id,
                ReturnCondition::Exit,
                SHORT,
                &CancellationToken::new(),
            ))
            .ok()
            .map(|outcome| outcome.outcome);
        (lifecycle, outcome)
    }

    fn lifecycle(&self, id: &str) -> Lifecycle {
        self.terminals
            .list(OWNER)
            .expect("list the terminals")
            .into_iter()
            .find(|facts| facts.session_id == id)
            .map(|facts| facts.lifecycle)
            .expect("the listed terminal")
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

fn bash_with_startup_files() -> ShellSpec {
    ShellSpec::Executable {
        path: PathBuf::from(BASH),
        clean_start: false,
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

fn child_pid(fixture: &Fixture) -> Pid {
    pid_in(fixture, "child")
}

fn pid_in(fixture: &Fixture, name: &str) -> Pid {
    fixture
        .file_until(name, |text| text.ends_with('\n'))
        .trim()
        .parse()
        .ok()
        .and_then(Pid::from_raw)
        .expect("a process id")
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
        (Lifecycle::Exited, Some(ReturnOutcome::Exited(7)))
    );
}

fn a_signal_ends_the_command() {
    let fixture = Fixture::new();
    let started = fixture.start("kill -KILL $$", ReturnCondition::Exit);
    assert_eq!(started.outcome, ReturnOutcome::Signal(SIGKILL));
    assert_eq!(started.facts.lifecycle, Lifecycle::Exited);
    assert_eq!(
        fixture.record(&started.id),
        (Lifecycle::Exited, Some(ReturnOutcome::Signal(SIGKILL)))
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
    let child = child_pid(&fixture);
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
    assert_eq!(
        fixture
            .records()
            .list(OWNER)
            .expect("list the saved records")
            .len(),
        1
    );
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

fn output_is_read_once_and_rendered_on_the_screen() {
    let fixture = Fixture::new();
    let started = fixture.start(
        "printf 'size %s\\n' \"$(stty size)\"; printf '\\033[31mred-text\\033[0m\\n'",
        ReturnCondition::Exit,
    );
    assert_eq!(started.outcome, ReturnOutcome::Exited(0));
    let read = fixture
        .terminals
        .read(OWNER, &started.id)
        .expect("read the terminal");
    let text = String::from_utf8_lossy(&read.output);
    assert!(text.contains("size 24 80"), "{text:?}");
    assert!(read.contains_escape);
    assert_eq!(read.total_bytes, read.output.len() as u64);
    assert!(!read.output_incomplete);
    let again = fixture
        .terminals
        .read(OWNER, &started.id)
        .expect("read the terminal again");
    assert!(again.output.is_empty());
    assert_eq!(again.total_bytes, 0);
    let screen = fixture
        .terminals
        .screen(OWNER, &started.id)
        .expect("render the screen");
    let screen = String::from_utf8_lossy(&screen.text);
    assert!(screen.contains("red-text"), "{screen:?}");
    assert!(screen.contains("size 24 80"), "{screen:?}");
    assert!(!screen.contains('\u{1b}'), "{screen:?}");
}

fn a_started_terminal_takes_input_until_it_exits() {
    let fixture = Fixture::new();
    let started = fixture.start(
        "read -r line; echo \"got $line\"; exit 4",
        ReturnCondition::Started,
    );
    assert_eq!(started.outcome, ReturnOutcome::Started);
    let written = fixture
        .terminals
        .write(OWNER, &started.id, "hello\n")
        .expect("write to the terminal");
    assert_eq!(written.accepted_bytes, 6);
    assert_eq!(fixture.wait_exit(&started.id), Ok(ReturnOutcome::Exited(4)));
    assert!(fixture.read_text(&started.id).contains("got hello"));
    assert_eq!(
        failure_code(fixture.terminals.write(OWNER, &started.id, "late\n")),
        StructuredErrorCode::InvalidLifecycle
    );
    assert_eq!(
        failure_code(fixture.terminals.write(STRANGER, &started.id, "x")),
        StructuredErrorCode::AuthorityDenied
    );
}

fn a_timeout_kills_the_command_and_marks_it_timed_out() {
    let fixture = Fixture::new();
    let started = fixture
        .start_with(
            "set +m; sleep 30",
            ReturnCondition::Exit,
            &bash(),
            Some(200),
        )
        .expect("start a terminal");
    assert_eq!(started.outcome, ReturnOutcome::Signal(SIGKILL));
    assert!(started.facts.timed_out);
    assert_eq!(
        fixture.record(&started.id),
        (Lifecycle::Exited, Some(ReturnOutcome::Signal(SIGKILL)))
    );
    assert!(
        fixture
            .terminals
            .list(OWNER)
            .expect("list the terminals")
            .iter()
            .any(|facts| facts.session_id == started.id && facts.timed_out)
    );
}

fn a_graceful_close_kills_a_command_that_ignores_terminate() {
    let fixture = Fixture::new();
    let started = fixture.start(
        "set +m; trap '' TERM; : > ready; while :; do sleep 1; done",
        ReturnCondition::Started,
    );
    fixture.file_until("ready", |_| true);
    let closing = Instant::now();
    let closed = fixture
        .close(&started.id, ClosePolicy::Graceful)
        .expect("close the terminal");
    assert!(closing.elapsed() >= GRACEFUL_CLOSE);
    assert_eq!(closed.lifecycle, Lifecycle::Closed);
    assert_eq!(fixture.lifecycle(&started.id), Lifecycle::Closed);
    assert_eq!(fixture.record(&started.id), (Lifecycle::Closed, None));
    for code in [
        failure_code(fixture.close(&started.id, ClosePolicy::Graceful)),
        failure_code(fixture.terminals.write(OWNER, &started.id, "x")),
        failure_code(fixture.terminals.read(OWNER, &started.id)),
        failure_code(fixture.terminals.screen(OWNER, &started.id)),
        failure_code(fixture.terminals.inspect(OWNER, &started.id)),
        failure_code(fixture.wait_exit(&started.id)),
    ] {
        assert_eq!(code, StructuredErrorCode::AuthorityDenied);
    }
}

fn a_timeout_during_a_graceful_close_ends_it_early() {
    let fixture = Fixture::new();
    let started = fixture
        .start_with(
            "set +m; trap '' TERM; : > ready; while :; do sleep 1; done",
            ReturnCondition::Started,
            &bash(),
            Some(300),
        )
        .expect("start a terminal");
    fixture.file_until("ready", |_| true);
    let closed = fixture
        .close(&started.id, ClosePolicy::Graceful)
        .expect("close the terminal");
    assert_eq!(closed.lifecycle, Lifecycle::Closed);
    assert!(closed.timed_out);
}

fn a_pending_wait_ends_when_its_terminal_closes() {
    let fixture = Fixture::new();
    let started = fixture.start(WAIT_FOR_GO, ReturnCondition::Started);
    let shared = fixture.terminals.clone();
    let id = started.id.clone();
    let (sender, outcomes) = mpsc::channel();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build a runtime");
    let waiter = thread::spawn(move || {
        let outcome = runtime.block_on(shared.wait(
            OWNER,
            &id,
            ReturnCondition::Exit,
            Duration::MAX,
            &CancellationToken::new(),
        ));
        let _ = sender.send(outcome.map(|outcome| outcome.outcome));
    });
    let closed = fixture
        .close(&started.id, ClosePolicy::Force)
        .expect("close the terminal");
    assert_eq!(closed.lifecycle, Lifecycle::Closed);
    let outcome = outcomes.recv_timeout(LONG).expect("the wait ends");
    let _ = waiter.join();
    assert!(
        match &outcome {
            Ok(outcome) => *outcome == ReturnOutcome::Signal(SIGKILL),
            Err(failure) => failure.code == StructuredErrorCode::AuthorityDenied,
        },
        "{outcome:?}"
    );
}

fn a_forced_close_ends_the_command_and_is_recorded() {
    let fixture = Fixture::new();
    let started = fixture.start(
        "set +m; sleep 30 & echo $! > child.tmp && mv child.tmp child; wait",
        ReturnCondition::Started,
    );
    let child = child_pid(&fixture);
    let closed = fixture
        .close(&started.id, ClosePolicy::Force)
        .expect("close the terminal");
    assert_eq!(closed.lifecycle, Lifecycle::Closed);
    let killed = gone_in_time(child);
    if !killed {
        let _ = kill_process(child, Signal::KILL);
    }
    assert!(killed, "the background child outlived the forced close");
    assert_eq!(
        fixture
            .terminals
            .list(OWNER)
            .expect("list the terminals")
            .iter()
            .map(|facts| (facts.session_id.as_str(), facts.lifecycle))
            .collect::<Vec<_>>(),
        [(started.id.as_str(), Lifecycle::Closed)]
    );
}

fn a_forced_close_ends_a_terminal_whose_input_is_full() {
    let fixture = Fixture::new();
    let started = fixture.start(
        "set +m; stty raw -echo; : > ready; sleep 30",
        ReturnCondition::Started,
    );
    fixture.file_until("ready", |_| true);
    let writer = {
        let terminals = fixture.terminals.clone();
        let id = started.id.clone();
        let input = "x".repeat(64 * 1024);
        thread::spawn(move || {
            loop {
                if let Err(failure) = terminals.write(OWNER, &id, &input) {
                    return failure.code;
                }
            }
        })
    };
    let blocked = Instant::now() + Duration::from_millis(300);
    while !writer.is_finished() && Instant::now() < blocked {
        thread::sleep(POLL);
    }
    assert!(!writer.is_finished(), "the writes ended before the close");
    let closing = Instant::now();
    let closed = fixture
        .close(&started.id, ClosePolicy::Force)
        .expect("close the terminal");
    assert!(closing.elapsed() < LONG);
    assert_eq!(closed.lifecycle, Lifecycle::Closed);
    assert_eq!(
        writer.join().expect("the writer ends"),
        StructuredErrorCode::AuthorityDenied
    );
    assert_eq!(fixture.lifecycle(&started.id), Lifecycle::Closed);
}

fn a_terminal_closed_while_input_waits_reads_only_what_it_accepted() {
    let fixture = Fixture::new();
    let started = fixture.start(
        "set +m; stty raw -echo; trap 'cat > drained' TERM; : > ready; while :; do sleep 1; done",
        ReturnCondition::Started,
    );
    fixture.file_until("ready", |_| true);
    let writer = {
        let terminals = fixture.terminals.clone();
        let id = started.id.clone();
        thread::spawn(move || {
            let mut accepted = 0;
            loop {
                match terminals.write(OWNER, &id, "x") {
                    Ok(_) => accepted += 1,
                    Err(failure) => return (accepted, failure.code),
                }
            }
        })
    };
    let filled = Instant::now() + STALLED;
    while !writer.is_finished() && Instant::now() < filled {
        thread::sleep(POLL);
    }
    let closed = fixture
        .close(&started.id, ClosePolicy::Graceful)
        .expect("close the terminal");
    assert_eq!(closed.lifecycle, Lifecycle::Closed);
    let (accepted, code) = writer.join().expect("the writer ends");
    assert_eq!(code, StructuredErrorCode::AuthorityDenied);
    let drained = fs::read(fixture.cwd.join("drained")).unwrap_or_default();
    assert!(
        drained.len() <= accepted,
        "the terminal read {} bytes, more than the {accepted} it accepted; the session unit test a_write_waiting_for_room_writes_nothing_once_the_close_begins checks the ordering",
        drained.len()
    );
}

fn saved_terminals_answer_after_their_process_ended() {
    let directory = tempfile::tempdir().expect("create a directory");
    let cwd = fs::canonicalize(directory.path()).expect("canonicalize the directory");
    let store = cwd.join("store");
    let report = scenario("saved", &cwd, &store, &[]);
    let ids: Vec<&str> = report.split_whitespace().collect();
    let [exited, ended] = ids[..] else {
        panic!("the scenario reported {report:?}");
    };
    let fixture = Fixture::at(Some(directory), store, cwd);
    assert_eq!(
        fixture
            .terminals
            .list(OWNER)
            .expect("list the terminals")
            .iter()
            .map(|facts| (facts.session_id.as_str(), facts.lifecycle))
            .collect::<Vec<_>>(),
        [(exited, Lifecycle::Exited), (ended, Lifecycle::Lost)]
    );
    assert_eq!(fixture.wait_exit(exited), Ok(ReturnOutcome::Exited(5)));
    assert_eq!(
        failure_code(fixture.wait(
            OWNER,
            exited,
            ReturnCondition::Started,
            LONG,
            &CancellationToken::new()
        )),
        StructuredErrorCode::SessionNotFound
    );
    assert_eq!(
        failure_code(fixture.wait_exit(ended)),
        StructuredErrorCode::SessionNotFound
    );
    let read = fixture
        .terminals
        .read(OWNER, exited)
        .expect("read the terminal");
    assert!(read.output.is_empty() && read.output_incomplete);
    assert_eq!(
        failure_code(fixture.terminals.screen(OWNER, exited)),
        StructuredErrorCode::ScreenUnavailable
    );
    assert_eq!(
        failure_code(fixture.terminals.write(OWNER, exited, "x")),
        StructuredErrorCode::SessionNotFound
    );
    a_stranger_is_denied_a_saved_terminal(&fixture, exited);
    assert_eq!(
        fixture
            .terminals
            .inspect(OWNER, exited)
            .expect("inspect the terminal")
            .command
            .as_deref(),
        Some("exit 5")
    );
    assert_eq!(
        fixture
            .close(ended, ClosePolicy::Graceful)
            .map(|facts| facts.lifecycle),
        Ok(Lifecycle::Closed)
    );
    for code in [
        failure_code(fixture.close(ended, ClosePolicy::Force)),
        failure_code(fixture.terminals.read(OWNER, ended)),
        failure_code(fixture.terminals.inspect(OWNER, ended)),
        failure_code(fixture.wait_exit(ended)),
    ] {
        assert_eq!(code, StructuredErrorCode::AuthorityDenied);
    }
}

fn a_stranger_is_denied_a_saved_terminal(fixture: &Fixture, id: &str) {
    for code in [
        failure_code(fixture.terminals.read(STRANGER, id)),
        failure_code(fixture.terminals.screen(STRANGER, id)),
        failure_code(fixture.terminals.inspect(STRANGER, id)),
        failure_code(fixture.wait(
            STRANGER,
            id,
            ReturnCondition::Exit,
            LONG,
            &CancellationToken::new(),
        )),
        failure_code(fixture.runtime.block_on(fixture.terminals.close(
            STRANGER,
            id,
            ClosePolicy::Force,
        ))),
    ] {
        assert_eq!(code, StructuredErrorCode::AuthorityDenied);
    }
}

fn the_screen_is_unavailable_after_output_it_cannot_parse() {
    let fixture = Fixture::new();
    let started = fixture.start(
        "printf 'before\\n\\033[;;;;;;;;;;;;;;;;;;;;mafter\\n'",
        ReturnCondition::Exit,
    );
    assert_eq!(started.outcome, ReturnOutcome::Exited(0));
    assert_eq!(
        failure_code(fixture.terminals.screen(OWNER, &started.id)),
        StructuredErrorCode::ScreenUnavailable
    );
    let text = fixture.read_text(&started.id);
    assert!(
        text.contains("before") && text.contains("after"),
        "{text:?}"
    );
}

fn requests_are_checked_before_anything_runs() {
    let fixture = Fixture::new();
    let invalid = |command: &str, timeout_ms| {
        failure_code(fixture.start_with(command, ReturnCondition::Exit, &bash(), timeout_ms))
    };
    assert_eq!(invalid("", None), StructuredErrorCode::InvalidRequest);
    assert_eq!(invalid("true\0", None), StructuredErrorCode::InvalidRequest);
    assert_eq!(
        invalid(&"x".repeat(64 * 1024 + 1), None),
        StructuredErrorCode::InvalidRequest
    );
    assert_eq!(
        invalid("true", Some(0)),
        StructuredErrorCode::InvalidRequest
    );
    assert_eq!(
        invalid("true", Some(u64::MAX)),
        StructuredErrorCode::InvalidRequest
    );
    assert!(
        fixture
            .terminals
            .list(OWNER)
            .expect("list the terminals")
            .is_empty()
    );
    let started = fixture.start(WAIT_FOR_GO, ReturnCondition::Started);
    assert_eq!(
        failure_code(fixture.terminals.write(OWNER, &started.id, "")),
        StructuredErrorCode::InvalidRequest
    );
    assert_eq!(
        failure_code(
            fixture
                .terminals
                .write(OWNER, &started.id, &"x".repeat(64 * 1024 + 1))
        ),
        StructuredErrorCode::InvalidRequest
    );
    assert_eq!(
        failure_code(fixture.wait(
            OWNER,
            &started.id,
            ReturnCondition::Exit,
            Duration::ZERO,
            &CancellationToken::new()
        )),
        StructuredErrorCode::InvalidRequest
    );
    for id in ["", "../escape", "shell id"] {
        assert_eq!(
            failure_code(fixture.terminals.read(OWNER, id)),
            StructuredErrorCode::InvalidRequest,
            "{id:?}"
        );
    }
    fs::write(fixture.cwd.join("go"), "").expect("let the command finish");
    assert_eq!(fixture.wait_exit(&started.id), Ok(ReturnOutcome::Exited(4)));
}

fn scenario(name: &str, home: &Path, store: &Path, environment: &[(&str, &Path)]) -> String {
    let (sender, reported) = mpsc::channel();
    let mut scenario = Command::new(env::current_exe().expect("the test executable"))
        .env(SCENARIO_VARIABLE, name)
        .env(STORE_VARIABLE, store)
        .env("HOME", home)
        .envs(environment.iter().copied())
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("run the scenario");
    let mut stdout = scenario.stdout.take().expect("the scenario's output");
    thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        let _ = sender.send(text);
    });
    let report = reported.recv_timeout(LONG);
    let _ = scenario.kill();
    let _ = scenario.wait();
    report.expect("the scenario reports in time")
}

fn run_scenario(name: &OsString) -> ExitCode {
    let store = PathBuf::from(env::var_os(STORE_VARIABLE).expect("the scenario's store"));
    let cwd = env::current_dir().expect("the scenario's directory");
    let fixture = Fixture::at(None, store, cwd);
    let report = if name == "saved" {
        let exited = fixture.start("exit 5", ReturnCondition::Exit);
        let ended = fixture.start("set +m; sleep 30", ReturnCondition::Started);
        fixture.end_owner();
        format!("{} {}", exited.id, ended.id)
    } else if name == "never-prepares" {
        let failure = fixture.start_in("true", ReturnCondition::Exit, &bash());
        let listed = fixture.terminals.list(OWNER).map(|listed| listed.len());
        format!("{:?} {listed:?}", failure_code(failure))
    } else if name == "profile" {
        let failure = fixture.start_in("true", ReturnCondition::Exit, &bash_with_startup_files());
        format!("{:?}", failure_code(failure))
    } else if name == "starting" {
        let started = fixture
            .runtime
            .block_on(fixture.terminals.start(
                &StartRequest {
                    owner_session_id: OWNER,
                    cwd: &fixture.cwd,
                    command: "true",
                    shell: &bash_with_startup_files(),
                    return_when: ReturnCondition::Exit,
                    wait_ceiling: STALLED,
                    timeout_ms: None,
                },
                &CancellationToken::new(),
            ))
            .expect("start a terminal");
        let closed = fixture
            .close(&started.session.session_id, ClosePolicy::Force)
            .map(|facts| facts.lifecycle);
        format!(
            "{:?} {:?} {closed:?}",
            started.outcome, started.session.lifecycle
        )
    } else if name == "crash" {
        fixture.start(
            "set +m; sleep 30 & echo $! > child.tmp && mv child.tmp child; wait",
            ReturnCondition::Started,
        );
        let child = child_pid(&fixture);
        let _ = writeln!(io::stdout(), "{child:?}");
        let _ = io::stdout().flush();
        std::process::exit(0);
    } else {
        format!(
            "{:?}",
            fixture.start("exit 0", ReturnCondition::Exit).outcome
        )
    };
    drop(fixture);
    let _ = writeln!(io::stdout(), "{report}");
    ExitCode::SUCCESS
}

fn consume_launcher_config() -> ExitCode {
    let mut stdin = io::stdin().lock();
    let mut length = [0; 4];
    let consumed = stdin.read_exact(&mut length).and_then(|()| {
        let mut config = vec![0; usize::try_from(u32::from_le_bytes(length)).unwrap_or(0)];
        stdin.read_exact(&mut config)
    });
    if consumed.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn record_control_socket_mode(args: &[OsString]) {
    let (Some(record), Some(socket)) = (env::var_os(SOCKET_MODE_VARIABLE), args.get(1)) else {
        return;
    };
    let Ok(metadata) = fs::symlink_metadata(socket) else {
        return;
    };
    let kind = if metadata.file_type().is_socket() {
        "socket"
    } else {
        "other"
    };
    let line = format!("{kind} {:o}\n", metadata.permissions().mode() & 0o7777);
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(record)
        .and_then(|mut file| file.write_all(line.as_bytes()));
}

fn the_command_is_the_foreground_job_of_its_terminal() {
    let fixture = Fixture::new();
    let started = fixture.start(
        "tty >/dev/null && [ \"$(ps -o tpgid= -p $$ | tr -d ' ')\" = \"$$\" ] || exit 1",
        ReturnCondition::Exit,
    );
    assert_eq!(started.outcome, ReturnOutcome::Exited(0));
}

fn a_shell_that_exits_during_startup_fails_the_start() {
    let (_home, home) = canonical_temp_dir();
    fs::write(home.join(".bash_profile"), "exit 9\n").expect("write a startup file");
    assert_eq!(
        scenario("profile", &home, &home.join("store"), &[]),
        format!("{:?}\n", StructuredErrorCode::StartupFailed)
    );
}

fn the_control_socket_is_private_to_its_owner() {
    let (_home, home) = canonical_temp_dir();
    let modes = home.join("modes");
    assert_eq!(
        scenario(
            "socket",
            &home,
            &home.join("store"),
            &[(SOCKET_MODE_VARIABLE, &modes)]
        ),
        format!("{:?}\n", ReturnOutcome::Exited(0))
    );
    assert_eq!(
        fs::read_to_string(&modes).expect("the recorded socket modes"),
        "socket 600\nsocket 600\n"
    );
}

fn a_launcher_that_never_prepares_fails_the_start() {
    let (_home, home) = canonical_temp_dir();
    assert_eq!(
        scenario(
            "never-prepares",
            &home,
            &home.join("store"),
            &[(NEVER_PREPARES_VARIABLE, &home)]
        ),
        format!("{:?} Ok(0)\n", StructuredErrorCode::PtyUnavailable)
    );
}

fn a_forced_close_ends_a_terminal_that_never_started() {
    let (_home, home) = canonical_temp_dir();
    fs::write(home.join(".bash_profile"), "read -r _\n").expect("write a startup file");
    assert_eq!(
        scenario("starting", &home, &home.join("store"), &[]),
        format!(
            "{:?} {:?} {:?}\n",
            ReturnOutcome::SafetyCeiling,
            Lifecycle::Starting,
            Ok::<_, TerminalFailure>(Lifecycle::Closed)
        )
    );
}

fn a_crashed_owner_leaves_no_terminal_behind() {
    let (_home, home) = canonical_temp_dir();
    let report = scenario("crash", &home, &home.join("store"), &[]);
    let child = report
        .trim()
        .strip_prefix("Pid(")
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(|raw| raw.parse().ok())
        .and_then(Pid::from_raw)
        .unwrap_or_else(|| panic!("the scenario reported {report:?}"));
    let killed = gone_in_time(child);
    if !killed {
        let _ = kill_process(child, Signal::KILL);
    }
    assert!(killed, "the background child outlived its crashed owner");
}

fn signals_reach_the_command_and_its_process_group() {
    let fixture = Fixture::new();
    let looping = fixture.start(
        "set +m; : > ready; while :; do sleep 1; done",
        ReturnCondition::Started,
    );
    fixture.file_until("ready", |_| true);
    let signalled = fixture
        .terminals
        .signal(OWNER, &looping.id, TerminalSignal::Hangup)
        .expect("hang up the terminal");
    assert_eq!(signalled.signal, TerminalSignal::Hangup);
    assert_eq!(
        fixture.wait_exit(&looping.id),
        Ok(ReturnOutcome::Signal(SIGHUP))
    );
    let plain = fixture.start(
        "set +m; sleep 30 & echo $! > child.tmp && mv child.tmp child; wait $!",
        ReturnCondition::Started,
    );
    let child = child_pid(&fixture);
    fixture
        .terminals
        .signal(OWNER, &plain.id, TerminalSignal::Terminate)
        .expect("terminate the terminal");
    assert_eq!(
        fixture.wait_exit(&plain.id),
        Ok(ReturnOutcome::Exited(128 + SIGTERM))
    );
    let killed = gone_in_time(child);
    if !killed {
        let _ = kill_process(child, Signal::KILL);
    }
    assert!(killed, "the background child outlived its terminate signal");
}

fn signals_need_a_live_terminal_of_their_owner() {
    let fixture = Fixture::new();
    let exited = fixture.start("exit 0", ReturnCondition::Exit);
    assert_eq!(
        failure_code(
            fixture
                .terminals
                .signal(OWNER, &exited.id, TerminalSignal::Kill)
        ),
        StructuredErrorCode::InvalidLifecycle
    );
    let running = fixture.start(WAIT_FOR_GO, ReturnCondition::Started);
    assert_eq!(
        failure_code(
            fixture
                .terminals
                .signal(STRANGER, &running.id, TerminalSignal::Kill)
        ),
        StructuredErrorCode::AuthorityDenied
    );
    assert_eq!(
        failure_code(fixture.terminals.signal(
            OWNER,
            "shell-AAAAAAAAAAAAAAAAAAAAAA",
            TerminalSignal::Kill
        )),
        StructuredErrorCode::SessionNotFound
    );
    fixture
        .close(&running.id, ClosePolicy::Force)
        .expect("close the terminal");
    assert_eq!(
        failure_code(
            fixture
                .terminals
                .signal(OWNER, &running.id, TerminalSignal::Kill)
        ),
        StructuredErrorCode::AuthorityDenied
    );
}

fn closes_reach_processes_that_left_the_process_group() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let fixture = Fixture::new();
    let started = fixture.start(
        "set +m; setsid sh -c 'echo $$ > child.tmp && mv child.tmp child && exec sleep 30' & wait",
        ReturnCondition::Started,
    );
    let child = child_pid(&fixture);
    fixture
        .close(&started.id, ClosePolicy::Force)
        .expect("close the terminal");
    let killed = gone_in_time(child);
    if !killed {
        let _ = kill_process(child, Signal::KILL);
    }
    assert!(killed, "the detached child outlived the forced close");
    let fixture = Fixture::new();
    fixture.start(
        "set +m; setsid sh -c 'echo $$ > child.tmp && mv child.tmp child && exec sleep 30' & wait",
        ReturnCondition::Started,
    );
    let child = child_pid(&fixture);
    fixture.end_owner();
    let killed = gone_in_time(child);
    if !killed {
        let _ = kill_process(child, Signal::KILL);
    }
    assert!(killed, "the detached child outlived its owner");
}

const SLEEPER: &str = "sleep 30";
const TRICKLER: &str =
    "perl -e '$| = 1; while (1) { print q(x); select(undef, undef, undef, 0.0002) }'";

fn held_after_its_shell_exits(fixture: &Fixture, job_control: &str, holder: &str) -> (String, Pid) {
    let started = fixture.start(
        &format!(
            "{job_control}; trap '' HUP; {holder} & echo $! > child.tmp && mv child.tmp child; \
             echo $$ > shell.tmp && mv shell.tmp shell; exit 0"
        ),
        ReturnCondition::Started,
    );
    let child = child_pid(fixture);
    assert!(
        gone_in_time(pid_in(fixture, "shell")),
        "the shell did not exit"
    );
    assert_eq!(fixture.lifecycle(&started.id), Lifecycle::Running);
    (started.id, child)
}

fn close_in_time(
    fixture: &Fixture,
    id: &str,
    policy: ClosePolicy,
) -> (Option<Result<Lifecycle, StructuredErrorCode>>, Duration) {
    let begun = Instant::now();
    let closed = fixture.runtime.block_on(async {
        tokio::time::timeout(LONG, fixture.terminals.close(OWNER, id, policy)).await
    });
    let answer = closed.ok().map(|closed| {
        closed
            .map(|closed| closed.session.lifecycle)
            .map_err(|failure| failure.code)
    });
    (answer, begun.elapsed())
}

fn a_forced_close_ends_the_group_holding_a_terminal_whose_shell_exited() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let fixture = Fixture::new();
    let (id, child) = held_after_its_shell_exits(&fixture, "set +m", SLEEPER);
    let (closed, took) = close_in_time(&fixture, &id, ClosePolicy::Force);
    let gone = gone_in_time(child);
    if !gone {
        let _ = kill_process(child, Signal::KILL);
    }
    assert_eq!(closed, Some(Ok(Lifecycle::Closed)));
    assert!(took < PROMPT_CLOSE, "the close took {took:?}");
    assert!(gone, "the process holding the terminal outlived the close");
}

fn a_close_ends_promptly_while_another_group_holds_the_terminal() {
    if !cfg!(target_os = "linux") {
        return;
    }
    for holder in [SLEEPER, TRICKLER] {
        for (policy, answer) in [
            (ClosePolicy::Force, Err(StructuredErrorCode::SessionLost)),
            (ClosePolicy::Graceful, Ok(Lifecycle::Closed)),
        ] {
            let directory = tempfile::tempdir().expect("create a directory");
            let cwd = fs::canonicalize(directory.path()).expect("canonicalize the directory");
            let fixture = Fixture::at(Some(directory), cwd.join("store"), cwd);
            let (id, child) = held_after_its_shell_exits(&fixture, "set -m", holder);
            let (closed, took) = close_in_time(&fixture, &id, policy);
            let _ = kill_process(child, Signal::KILL);
            assert_eq!(closed, Some(answer), "{holder} {policy:?}");
            assert_eq!(
                fixture.record(&id).0,
                Lifecycle::Closed,
                "{holder} {policy:?}"
            );
            assert!(
                took < PROMPT_CLOSE,
                "the {policy:?} close took {took:?} while {holder} held the terminal"
            );
        }
    }
}

fn canonical_temp_dir() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("create a directory");
    let path = fs::canonicalize(directory.path()).expect("canonicalize the directory");
    (directory, path)
}
