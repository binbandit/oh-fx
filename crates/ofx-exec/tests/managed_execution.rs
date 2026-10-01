use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ofx_exec::{
    CommandStatus, Environment, ExecutionError, ManagedExecutions, SessionSupervisor, Snapshot,
    SnapshotState, StartCaptured, is_foreground_session_invocation, run_foreground_session,
};
use tokio_util::sync::CancellationToken;

const OWNER_CHILD_VARIABLE: &str = "OH_FX_TEST_OWNER_DIRECTORY";
const BASH: &str = "/bin/bash";
const LONG: Duration = Duration::from_secs(20);

type Test = fn();

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if is_foreground_session_invocation(&args) {
        run_foreground_session(&args);
    }
    if let Some(directory) = env::var_os(OWNER_CHILD_VARIABLE) {
        owner_child(Path::new(&directory));
    }
    let filters: Vec<String> = args
        .iter()
        .filter_map(|argument| argument.to_str())
        .filter(|argument| !argument.starts_with('-'))
        .map(str::to_owned)
        .collect();
    let tests: [(&str, Test); 14] = [
        (
            "a_fast_command_completes_inside_its_yield_window",
            a_fast_command_completes_inside_its_yield_window,
        ),
        (
            "a_slow_command_yields_a_retained_session_that_stop_ends",
            a_slow_command_yields_a_retained_session_that_stop_ends,
        ),
        (
            "observing_returns_only_output_produced_since_the_last_delivery",
            observing_returns_only_output_produced_since_the_last_delivery,
        ),
        (
            "a_deadline_stops_the_command_with_timeout_expired",
            a_deadline_stops_the_command_with_timeout_expired,
        ),
        (
            "retained_output_is_capped_while_byte_counts_stay_exact",
            retained_output_is_capped_while_byte_counts_stay_exact,
        ),
        (
            "natural_completion_kills_background_jobs_left_in_the_command_group",
            natural_completion_kills_background_jobs_left_in_the_command_group,
        ),
        (
            "cancelling_the_yield_window_stops_the_unpublished_command",
            cancelling_the_yield_window_stops_the_unpublished_command,
        ),
        (
            "a_graceful_stop_escalates_when_the_command_ignores_sigterm",
            a_graceful_stop_escalates_when_the_command_ignores_sigterm,
        ),
        (
            "launch_failures_keep_upstream_error_names",
            launch_failures_keep_upstream_error_names,
        ),
        (
            "commands_have_no_controlling_terminal",
            commands_have_no_controlling_terminal,
        ),
        (
            "shutdown_stops_every_live_command_and_refuses_new_ones",
            shutdown_stops_every_live_command_and_refuses_new_ones,
        ),
        (
            "unknown_sessions_are_not_found",
            unknown_sessions_are_not_found,
        ),
        (
            "losing_the_supervisor_kills_the_command_group",
            losing_the_supervisor_kills_the_command_group,
        ),
        (
            "losing_the_owner_kills_the_command_group",
            losing_the_owner_kills_the_command_group,
        ),
    ];
    let mut failed = Vec::new();
    for (name, test) in tests {
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

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the test step succeeds")
        .block_on(future)
}

fn executions() -> ManagedExecutions {
    ManagedExecutions::new(SessionSupervisor::new(
        env::current_exe().expect("the test step succeeds"),
    ))
}

fn run(command: &str, yield_time: Duration) -> StartCaptured {
    StartCaptured {
        command: command.to_owned(),
        cwd: env::temp_dir(),
        environment: Environment::Clean(BASH.into()),
        max_output_bytes: 64 * 1024,
        timeout: None,
        yield_time,
    }
}

fn text(snapshot: &Snapshot) -> String {
    String::from_utf8_lossy(&snapshot.output_delta).into_owned()
}

fn fifo(directory: &Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    let made = process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .expect("the test step succeeds");
    assert!(made.success());
    path
}

fn read_fifo_in_background(path: PathBuf) -> std::sync::mpsc::Receiver<String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let _ = fs::File::open(path).and_then(|mut file| file.read_to_string(&mut text));
        let _ = sender.send(text);
    });
    receiver
}

async fn observe_until(
    executions: &ManagedExecutions,
    execution_id: &str,
    done: impl Fn(&Snapshot, &str) -> bool,
) -> (Snapshot, String) {
    let started = Instant::now();
    let mut seen = String::new();
    loop {
        let snapshot = executions
            .wait(
                execution_id,
                Duration::from_millis(50),
                &CancellationToken::new(),
            )
            .await
            .expect("the test step succeeds");
        seen.push_str(&text(&snapshot));
        if done(&snapshot, &seen) {
            return (snapshot, seen);
        }
        assert!(
            started.elapsed() < LONG,
            "never observed the expected output: {seen:?}"
        );
    }
}

fn a_fast_command_completes_inside_its_yield_window() {
    let snapshot = block_on(async {
        executions()
            .start_captured(
                run("printf out; printf err >&2; exit 3", LONG),
                &CancellationToken::new(),
            )
            .await
            .expect("the test step succeeds")
    });
    assert_eq!(
        snapshot.state,
        SnapshotState::Completed(CommandStatus::ExitCode(3))
    );
    assert!(!snapshot.retained);
    assert_eq!(snapshot.execution_id, "shell-1");
    assert!(text(&snapshot).contains("out"));
    assert!(text(&snapshot).contains("err"));
    assert_eq!((snapshot.stdout_bytes, snapshot.stderr_bytes), (3, 3));
    assert!(snapshot.duration_ms.is_some());
    assert_eq!(snapshot.error_name, None);
    assert!(!snapshot.output_truncated);
}

fn a_slow_command_yields_a_retained_session_that_stop_ends() {
    block_on(async {
        let executions = executions();
        let cancel = CancellationToken::new();
        let first = executions
            .start_captured(run("exec sleep 60", Duration::from_millis(300)), &cancel)
            .await
            .expect("the test step succeeds");
        assert_eq!(first.state, SnapshotState::Running);
        assert!(first.retained);
        assert_eq!(
            executions.command(&first.execution_id).as_deref(),
            Some("exec sleep 60")
        );
        assert_eq!(executions.tombstone_snapshot(&first.execution_id), None);
        let stopped = executions
            .stop(&first.execution_id, false)
            .await
            .expect("the test step succeeds");
        assert_eq!(
            stopped.state,
            SnapshotState::Stopped(Some(CommandStatus::Signal(15)))
        );
        assert!(stopped.retained);
        let again = executions
            .stop(&first.execution_id, true)
            .await
            .expect("the test step succeeds");
        assert_eq!(again.state, stopped.state);
        assert!(again.output_delta.is_empty());
        let retained = executions
            .tombstone_snapshot(&first.execution_id)
            .expect("the test step succeeds");
        assert_eq!(retained.state, stopped.state);
    });
}

fn observing_returns_only_output_produced_since_the_last_delivery() {
    block_on(async {
        let directory = tempfile::tempdir().expect("the test step succeeds");
        let gate = fifo(directory.path(), "gate");
        let executions = executions();
        let command = format!(
            "printf first; read line < {}; printf second",
            gate.display()
        );
        let first = executions
            .start_captured(run(&command, Duration::ZERO), &CancellationToken::new())
            .await
            .expect("the test step succeeds");
        assert_eq!(first.state, SnapshotState::Running);
        assert!(first.retained);
        let (_, seen) =
            observe_until(&executions, &first.execution_id, |_, seen| seen == "first").await;
        assert_eq!(seen, "first");
        let (sender, opened) = std::sync::mpsc::channel();
        let path = gate.clone();
        thread::spawn(move || {
            let _ = sender.send(fs::write(path, "go\n").is_ok());
        });
        assert_eq!(opened.recv_timeout(LONG), Ok(true));
        let (last, seen) = observe_until(&executions, &first.execution_id, |snapshot, _| {
            snapshot.state != SnapshotState::Running
        })
        .await;
        assert_eq!(
            last.state,
            SnapshotState::Completed(CommandStatus::ExitCode(0))
        );
        assert_eq!(seen, "second");
        assert!(last.retained);
        let (empty, seen) = observe_until(&executions, &first.execution_id, |_, _| true).await;
        assert!(empty.output_delta.is_empty());
        assert_eq!(seen, "");
        assert_eq!(empty.state, last.state);
    });
}

fn a_deadline_stops_the_command_with_timeout_expired() {
    let snapshot = block_on(async {
        let input = StartCaptured {
            timeout: Some(Duration::from_secs(2)),
            ..run("printf partial; exec sleep 60", LONG)
        };
        executions()
            .start_captured(input, &CancellationToken::new())
            .await
            .expect("the test step succeeds")
    });
    assert_eq!(snapshot.state, SnapshotState::Stopped(None));
    assert_eq!(snapshot.error_name, Some("TimeoutExpired"));
    assert_eq!(text(&snapshot), "partial");
    assert_eq!(snapshot.duration_ms, None);

    let immediate = block_on(async {
        let input = StartCaptured {
            timeout: Some(Duration::ZERO),
            ..run("printf never", LONG)
        };
        executions()
            .start_captured(input, &CancellationToken::new())
            .await
            .expect("the test step succeeds")
    });
    assert_eq!(immediate.state, SnapshotState::Stopped(None));
    assert_eq!(immediate.error_name, Some("TimeoutExpired"));
    assert!(immediate.output_delta.is_empty());
}

fn retained_output_is_capped_while_byte_counts_stay_exact() {
    let snapshot = block_on(async {
        let input = StartCaptured {
            max_output_bytes: 64,
            ..run("head -c 100000 /dev/zero | tr '\\000' a", LONG)
        };
        executions()
            .start_captured(input, &CancellationToken::new())
            .await
            .expect("the test step succeeds")
    });
    assert_eq!(
        snapshot.state,
        SnapshotState::Completed(CommandStatus::ExitCode(0))
    );
    assert_eq!(snapshot.output_delta, vec![b'a'; 64]);
    assert!(snapshot.output_truncated);
    assert_eq!(snapshot.stdout_bytes, 100_000);
}

fn natural_completion_kills_background_jobs_left_in_the_command_group() {
    let directory = tempfile::tempdir().expect("the test step succeeds");
    let held = fifo(directory.path(), "held");
    let closed = read_fifo_in_background(held.clone());
    let command = format!("exec 3> {}; sleep 300 >&3 3>&- &", held.display());
    let snapshot = block_on(async {
        executions()
            .start_captured(run(&command, LONG), &CancellationToken::new())
            .await
            .expect("the test step succeeds")
    });
    assert_eq!(
        snapshot.state,
        SnapshotState::Completed(CommandStatus::ExitCode(0))
    );
    assert_eq!(
        closed.recv_timeout(LONG).ok().as_deref(),
        Some(""),
        "the background job kept its pipe open"
    );
}

fn cancelling_the_yield_window_stops_the_unpublished_command() {
    let directory = tempfile::tempdir().expect("the test step succeeds");
    let ready = fifo(directory.path(), "ready");
    let started = read_fifo_in_background(ready.clone());
    let command = format!("printf up > {}; exec sleep 60", ready.display());
    let snapshot = block_on(async {
        let executions = executions();
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let watcher = tokio::task::spawn_blocking(move || {
            let text = started.recv_timeout(LONG).expect("the test step succeeds");
            trigger.cancel();
            text
        });
        let snapshot = executions
            .start_captured(run(&command, LONG), &cancel)
            .await
            .expect("the test step succeeds");
        assert_eq!(watcher.await.expect("the test step succeeds"), "up");
        snapshot
    });
    assert_eq!(
        snapshot.state,
        SnapshotState::Stopped(Some(CommandStatus::Signal(15)))
    );
    assert!(!snapshot.retained);
}

fn a_graceful_stop_escalates_when_the_command_ignores_sigterm() {
    let directory = tempfile::tempdir().expect("the test step succeeds");
    let ready = fifo(directory.path(), "ready");
    let started = read_fifo_in_background(ready.clone());
    let command = format!("trap '' TERM; printf up > {}; sleep 60", ready.display());
    block_on(async {
        let executions = executions();
        let cancel = CancellationToken::new();
        let first = executions
            .start_captured(run(&command, Duration::ZERO), &cancel)
            .await
            .expect("the test step succeeds");
        assert_eq!(first.state, SnapshotState::Running);
        let text = tokio::task::spawn_blocking(move || {
            started.recv_timeout(LONG).expect("the test step succeeds")
        })
        .await
        .expect("the test step succeeds");
        assert_eq!(text, "up");
        let begun = Instant::now();
        let stopped = executions
            .stop(&first.execution_id, false)
            .await
            .expect("the test step succeeds");
        assert_eq!(
            stopped.state,
            SnapshotState::Stopped(Some(CommandStatus::Signal(9)))
        );
        assert!(
            begun.elapsed() >= Duration::from_millis(600),
            "{:?}",
            begun.elapsed()
        );
    });
}

fn launch_failures_keep_upstream_error_names() {
    block_on(async {
        let executions = executions();
        let cancel = CancellationToken::new();
        let missing_shell = StartCaptured {
            environment: Environment::Clean("/nonexistent/bash".into()),
            ..run("true", LONG)
        };
        let snapshot = executions
            .start_captured(missing_shell, &cancel)
            .await
            .expect("the test step succeeds");
        assert_eq!(snapshot.state, SnapshotState::Lost);
        assert_eq!(snapshot.error_name, Some("FileNotFound"));
        assert!(snapshot.output_delta.is_empty());
        let missing_cwd = StartCaptured {
            cwd: "/nonexistent/directory".into(),
            ..run("true", LONG)
        };
        let snapshot = executions
            .start_captured(missing_cwd, &cancel)
            .await
            .expect("the test step succeeds");
        assert_eq!(snapshot.state, SnapshotState::Lost);
        assert_eq!(snapshot.error_name, Some("FileNotFound"));
    });
}

fn commands_have_no_controlling_terminal() {
    let snapshot = block_on(async {
        executions()
            .start_captured(
                run("exec 3</dev/tty && printf opened", LONG),
                &CancellationToken::new(),
            )
            .await
            .expect("the test step succeeds")
    });
    assert!(
        matches!(snapshot.state, SnapshotState::Completed(CommandStatus::ExitCode(code)) if code != 0)
    );
    assert!(!text(&snapshot).contains("opened"));
}

fn shutdown_stops_every_live_command_and_refuses_new_ones() {
    let directory = tempfile::tempdir().expect("the test step succeeds");
    block_on(async {
        let executions = executions();
        let cancel = CancellationToken::new();
        let mut ids = Vec::new();
        for index in 0..3 {
            let ready = fifo(directory.path(), &format!("ready-{index}"));
            let started = read_fifo_in_background(ready.clone());
            let command = format!("printf up > {}; exec sleep 60", ready.display());
            let snapshot = executions
                .start_captured(run(&command, Duration::ZERO), &cancel)
                .await
                .expect("the test step succeeds");
            let text = tokio::task::spawn_blocking(move || started.recv_timeout(LONG))
                .await
                .expect("the test step succeeds");
            assert_eq!(text.as_deref(), Ok("up"));
            ids.push(snapshot.execution_id);
        }
        let begun = Instant::now();
        executions.shutdown().await;
        assert!(begun.elapsed() < Duration::from_secs(5));
        for id in ids {
            let snapshot = executions
                .wait(&id, Duration::ZERO, &cancel)
                .await
                .expect("the test step succeeds");
            assert_eq!(
                snapshot.state,
                SnapshotState::Stopped(Some(CommandStatus::Signal(15))),
                "{snapshot:?}"
            );
        }
        assert_eq!(
            executions.start_captured(run("true", LONG), &cancel).await,
            Err(ExecutionError::RuntimeStopping)
        );
    });
}

fn unknown_sessions_are_not_found() {
    block_on(async {
        let executions = executions();
        let cancel = CancellationToken::new();
        assert_eq!(
            executions.wait("shell-404", Duration::ZERO, &cancel).await,
            Err(ExecutionError::ExecutionNotFound)
        );
        assert_eq!(
            executions.stop("shell-404", true).await,
            Err(ExecutionError::ExecutionNotFound)
        );
        assert_eq!(executions.command("shell-404"), None);
    });
}

fn losing_the_supervisor_kills_the_command_group() {
    let directory = tempfile::tempdir().expect("the test step succeeds");
    let held = fifo(directory.path(), "held");
    let closed = read_fifo_in_background(held.clone());
    let command = format!(
        "exec 3> {}; sleep 300 >&3 3>&- & kill -KILL $PPID; wait",
        held.display()
    );
    let snapshot = block_on(async {
        executions()
            .start_captured(run(&command, LONG), &CancellationToken::new())
            .await
            .expect("the test step succeeds")
    });
    assert_eq!(
        snapshot.state,
        SnapshotState::Completed(CommandStatus::Signal(9))
    );
    assert_eq!(
        closed.recv_timeout(LONG).ok().as_deref(),
        Some(""),
        "the command group outlived its supervisor"
    );
}

fn losing_the_owner_kills_the_command_group() {
    let directory = tempfile::tempdir().expect("the test step succeeds");
    let ready = fifo(directory.path(), "ready");
    let held = fifo(directory.path(), "held");
    let started = read_fifo_in_background(ready);
    let closed = read_fifo_in_background(held);
    let mut owner = process::Command::new(env::current_exe().expect("the test step succeeds"))
        .env(OWNER_CHILD_VARIABLE, directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("the test step succeeds");
    let begun = started.recv_timeout(LONG).ok();
    owner.kill().expect("the test step succeeds");
    owner.wait().expect("the test step succeeds");
    assert_eq!(
        begun.as_deref(),
        Some("up"),
        "the owner never started its command"
    );
    assert_eq!(
        closed.recv_timeout(LONG).ok().as_deref(),
        Some(""),
        "the command group outlived its owner"
    );
}

fn owner_child(directory: &Path) -> ! {
    let command = format!(
        "exec 3> {}; sleep 300 >&3 3>&- & printf up > {}; wait",
        directory.join("held").display(),
        directory.join("ready").display()
    );
    block_on(async {
        let executions = executions();
        let snapshot = executions
            .start_captured(run(&command, Duration::ZERO), &CancellationToken::new())
            .await
            .expect("the test step succeeds");
        assert_eq!(snapshot.state, SnapshotState::Running);
        std::future::pending::<()>().await;
    });
    process::exit(1)
}
