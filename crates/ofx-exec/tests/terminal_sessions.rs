use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ofx_exec::{
    Action, ClosePolicy, Lifecycle, ReturnCondition, ReturnOutcome, SessionFacts, ShellSpec,
    StartRequest, StructuredErrorCode, TerminalFailure, TerminalStore, Terminals,
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
const SAVED_SCENARIO_VARIABLE: &str = "OH_FX_TEST_TERMINAL_SAVED_STORE";
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
];

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if is_terminal_control_invocation(&args) {
        run_terminal_control(&args);
    }
    if is_terminal_launcher_invocation(&args) {
        run_terminal_launcher();
    }
    if let Some(store) = env::var_os(SAVED_SCENARIO_VARIABLE) {
        return run_saved_scenario(PathBuf::from(store));
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
    fixture
        .file_until("child", |text| text.ends_with('\n'))
        .trim()
        .parse()
        .ok()
        .and_then(Pid::from_raw)
        .expect("the background child's pid")
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
    let report = saved_scenario(&cwd, &store);
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

fn saved_scenario(cwd: &Path, store: &Path) -> String {
    let (sender, reported) = mpsc::channel();
    let mut scenario = Command::new(env::current_exe().expect("the test executable"))
        .env(SAVED_SCENARIO_VARIABLE, store)
        .current_dir(cwd)
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

fn run_saved_scenario(store: PathBuf) -> ExitCode {
    let cwd = env::current_dir().expect("the scenario's directory");
    let fixture = Fixture::at(None, store, cwd);
    let exited = fixture.start("exit 5", ReturnCondition::Exit);
    let ended = fixture.start("set +m; sleep 30", ReturnCondition::Started);
    fixture.end_owner();
    drop(fixture);
    let _ = writeln!(io::stdout(), "{} {}", exited.id, ended.id);
    ExitCode::SUCCESS
}
