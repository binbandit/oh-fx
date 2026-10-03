use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ofx_exec::{
    CommandStatus, Environment, ExecutionError, HeldDirectory, ManagedExecutions, OutputEcho,
    SessionSupervisor, Snapshot, SnapshotState, StartCaptured, is_foreground_session_invocation,
    run_foreground_session,
};
use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use tokio_util::sync::CancellationToken;

const OWNER_CHILD_VARIABLE: &str = "OH_FX_TEST_OWNER_DIRECTORY";
const BASH: &str = "/bin/bash";
const LONG: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(10);
const SUPERVISOR_TOKEN: &str = "__oh_fx_foreground_session__";
const SUPERVISOR_RELEASE: &[u8] = b"0123456789abcdef0123456789abcdef\x06";
const NO_SUPERVISOR_DEADLINE: &str = "none";
const SUPERVISOR_DEADLINE_MILLISECONDS: &str = "3000";
const ESCAPED_PID: &str = "escaped.pid";
const STATUS_FRAME: &[u8] = b"\0OH_FX_FOREGROUND_STATUS:";
const PROMPT: Duration = Duration::from_millis(350);
const LEFT_SESSION: &str = "import os, sys, time
os.setsid()
with open(sys.argv[1] + '.tmp', 'w') as f: f.write(str(os.getpid()))
os.rename(sys.argv[1] + '.tmp', sys.argv[1])
time.sleep(60)
";
const MAIN_THREAD_EXITED: &str = "import ctypes, os, sys, threading, time
def state():
    return open('/proc/%d/stat' % os.getpid()).read().rsplit(')', 1)[1].split()[0]
def publish():
    while state() != 'Z': time.sleep(0.001)
    with open(sys.argv[1] + '.tmp', 'w') as f: f.write(str(os.getpid()))
    os.rename(sys.argv[1] + '.tmp', sys.argv[1])
    while True: time.sleep(1)
if os.fork() == 0:
    os.setsid()
    threading.Thread(target=publish).start()
    ctypes.CDLL(None).pthread_exit(None)
while True: time.sleep(1)
";
const IGNORES_SIGTERM: &str = "import os, signal, sys, time
signal.signal(signal.SIGTERM, lambda signum, frame: os._exit(0))
if os.fork() == 0:
    os.setsid()
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    with open(sys.argv[1] + '.tmp', 'w') as f: f.write(str(os.getpid()))
    os.rename(sys.argv[1] + '.tmp', sys.argv[1])
    while True: time.sleep(1)
while True: time.sleep(1)
";
const FOREIGN_PROC: &str = r#"import os, signal, subprocess, sys, time
def state(pid):
    try:
        return open('/proc/%s/stat' % pid).read().rsplit(')', 1)[1].split()[0]
    except (OSError, IndexError):
        return 'gone'
if sys.argv[1] == 'outer':
    ready_r, ready_w = os.pipe()
    decoy = os.fork()
    if decoy == 0:
        for _ in range(8):
            if os.fork() == 0:
                os.setsid()
                time.sleep(60)
                os._exit(0)
        os.write(ready_w, b'R')
        time.sleep(60)
        os._exit(0)
    os.read(ready_r, 1)
    children = open('/proc/%d/task/%d/children' % (decoy, decoy)).read().split()
    command = ['unshare', '--pid', '--fork', '--kill-child', sys.executable, sys.argv[0], 'inner']
    sys.exit(subprocess.run(command + [sys.argv[2], ','.join(children)]).returncode)
recorded = os.path.join(os.path.dirname(sys.argv[0]), 'target')
target = "import os, sys, time\nopen(sys.argv[1], 'w').write(os.readlink('/proc/self'))\ntime.sleep(60)"
supervisor = subprocess.Popen(
    [sys.argv[2], '__oh_fx_foreground_session__', 'none', '%d:%d' % (os.stat('.').st_dev, os.stat('.').st_ino), sys.executable, '-c', target, recorded],
    stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
if supervisor.stderr.read(1) != b'\x1e':
    sys.exit(1)
supervisor.stdin.write(b'0123456789abcdef0123456789abcdef\x06')
supervisor.stdin.flush()
victims = [subprocess.Popen(['sleep', '60'], start_new_session=True) for _ in range(6)]
decoys = set(int(pid) for pid in sys.argv[3].split(','))
print('arranged=' + ('yes' if any(victim.pid in decoys for victim in victims) else 'no'))
while not (os.path.exists(recorded) and open(recorded).read()):
    time.sleep(0.01)
outer_pid = open(recorded).read()
os.kill(supervisor.pid, signal.SIGUSR1)
supervisor.wait()
waited = time.time() + 10
while state(outer_pid) not in ('Z', 'gone') and time.time() < waited:
    time.sleep(0.01)
print('target=' + state(outer_pid))
time.sleep(0.2)
print('killed=%s' % [victim.pid for victim in victims if victim.poll() is not None])
for victim in victims:
    victim.kill()
"#;

type Test = fn();

const TESTS: [(&str, Test); 28] = [
    (
        "a_fast_command_completes_inside_its_yield_window",
        a_fast_command_completes_inside_its_yield_window,
    ),
    (
        "echoed_output_arrives_in_whole_lines_before_the_command_completes",
        echoed_output_arrives_in_whole_lines_before_the_command_completes,
    ),
    (
        "a_slow_command_yields_a_retained_session_that_stop_ends",
        a_slow_command_yields_a_retained_session_that_stop_ends,
    ),
    (
        "a_separate_runtime_shares_only_the_supervisor",
        a_separate_runtime_shares_only_the_supervisor,
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
        "commands_run_only_in_the_held_directory",
        commands_run_only_in_the_held_directory,
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
    (
        "a_forced_stop_kills_a_direct_command_that_left_the_session",
        a_forced_stop_kills_a_direct_command_that_left_the_session,
    ),
    (
        "losing_the_owner_kills_a_direct_command_that_left_the_session",
        losing_the_owner_kills_a_direct_command_that_left_the_session,
    ),
    (
        "a_passed_deadline_kills_a_direct_command_that_left_the_session",
        a_passed_deadline_kills_a_direct_command_that_left_the_session,
    ),
    (
        "abandoned_starts_release_their_slot_once_they_settle",
        abandoned_starts_release_their_slot_once_they_settle,
    ),
    (
        "a_detached_daemon_holding_the_output_ends_the_drain_with_complete_output",
        a_detached_daemon_holding_the_output_ends_the_drain_with_complete_output,
    ),
    (
        "a_forced_stop_kills_a_descendant_whose_main_thread_exited",
        a_forced_stop_kills_a_descendant_whose_main_thread_exited,
    ),
    (
        "a_graceful_stop_kills_a_descendant_whose_main_thread_exited",
        a_graceful_stop_kills_a_descendant_whose_main_thread_exited,
    ),
    (
        "losing_the_owner_kills_a_descendant_whose_main_thread_exited",
        losing_the_owner_kills_a_descendant_whose_main_thread_exited,
    ),
    (
        "a_forced_stop_through_another_namespaces_proc_spares_unrelated_processes",
        a_forced_stop_through_another_namespaces_proc_spares_unrelated_processes,
    ),
    (
        "forcing_a_graceful_stop_after_the_command_exited_kills_what_ignores_sigterm",
        forcing_a_graceful_stop_after_the_command_exited_kills_what_ignores_sigterm,
    ),
    (
        "losing_the_owner_after_the_command_exited_kills_what_ignores_sigterm",
        losing_the_owner_after_the_command_exited_kills_what_ignores_sigterm,
    ),
];

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
        cwd_directory: held(&env::temp_dir()),
        environment: Environment::Clean(BASH.into()),
        max_output_bytes: 64 * 1024,
        timeout: None,
        yield_time,
    }
}

fn held(directory: &Path) -> HeldDirectory {
    let directory = fs::File::open(directory).expect("the test step succeeds");
    HeldDirectory::new(directory.into())
}

fn supervisor_identity() -> String {
    let metadata = fs::metadata(".").expect("the test step succeeds");
    format!("{}:{}", metadata.dev(), metadata.ino())
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

async fn poll_once<F: Future + Unpin>(future: &mut F) -> Option<F::Output> {
    tokio::select! {
        biased;
        output = future => Some(output),
        () = std::future::ready(()) => None,
    }
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

fn echoed_output_arrives_in_whole_lines_before_the_command_completes() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let echo: OutputEcho = Arc::new(move |chunk: &[u8]| {
        recorded
            .lock()
            .expect("the test step succeeds")
            .push(String::from_utf8_lossy(chunk).into_owned());
        Ok(())
    });
    let echoed_lines = || {
        let mut lines: Vec<String> = seen
            .lock()
            .expect("the test step succeeds")
            .iter()
            .flat_map(|chunk| chunk.lines().map(str::to_owned).collect::<Vec<_>>())
            .collect();
        lines.sort_unstable();
        lines
    };
    block_on(async {
        let directory = tempfile::tempdir().expect("the test step succeeds");
        let gate = fifo(directory.path(), "gate");
        let executions = executions().with_output_echo(echo);
        let command = format!(
            "printf 'err\\n' >&2; printf 'one\\ntwo\\n'; read line < {}; printf 'three'",
            gate.display()
        );
        let first = executions
            .start_captured(run(&command, Duration::ZERO), &CancellationToken::new())
            .await
            .expect("the test step succeeds");
        assert_eq!(first.state, SnapshotState::Running);
        let started = Instant::now();
        while echoed_lines() != ["err", "one", "two"] {
            assert!(
                started.elapsed() < LONG,
                "the running command's lines were not echoed: {:?}",
                echoed_lines()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (sender, opened) = std::sync::mpsc::channel();
        let path = gate.clone();
        thread::spawn(move || {
            let _ = sender.send(fs::write(path, "go\n").is_ok());
        });
        assert_eq!(opened.recv_timeout(LONG), Ok(true));
        let (last, _) = observe_until(&executions, &first.execution_id, |snapshot, _| {
            snapshot.state != SnapshotState::Running
        })
        .await;
        assert_eq!(
            last.state,
            SnapshotState::Completed(CommandStatus::ExitCode(0))
        );
    });
    let mut chunks = seen.lock().expect("the test step succeeds").clone();
    assert_eq!(chunks.last().map(String::as_str), Some("three"));
    chunks.pop();
    assert!(
        chunks.iter().all(|chunk| chunk.ends_with('\n')),
        "{chunks:?}"
    );
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

fn a_separate_runtime_shares_only_the_supervisor() {
    let echoed = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&echoed);
    let echo: OutputEcho = Arc::new(move |chunk: &[u8]| {
        recorded
            .lock()
            .expect("the test step succeeds")
            .extend_from_slice(chunk);
        Ok(())
    });
    block_on(async {
        let parent = executions().with_output_echo(echo);
        let cancel = CancellationToken::new();
        let own = parent
            .start_captured(run("exec sleep 60", Duration::ZERO), &cancel)
            .await
            .expect("the test step succeeds");
        let child = parent.separate();
        let started = child
            .start_captured(
                run(
                    "printf 'child\\n'; exec sleep 60",
                    Duration::from_millis(300),
                ),
                &cancel,
            )
            .await
            .expect("the test step succeeds");
        assert_eq!(started.execution_id, own.execution_id);
        assert_eq!(text(&started), "child\n");
        child.shutdown().await;
        let stopped = child
            .wait(&started.execution_id, Duration::ZERO, &cancel)
            .await
            .expect("the test step succeeds");
        assert_eq!(
            stopped.state,
            SnapshotState::Stopped(Some(CommandStatus::Signal(15)))
        );
        let untouched = parent
            .wait(&own.execution_id, Duration::ZERO, &cancel)
            .await
            .expect("the test step succeeds");
        assert_eq!(untouched.state, SnapshotState::Running);
        assert!(
            parent
                .start_captured(run("true", LONG), &cancel)
                .await
                .is_ok()
        );
        parent.shutdown().await;
    });
    assert!(echoed.lock().expect("the test step succeeds").is_empty());
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
    });
}

fn commands_run_only_in_the_held_directory() {
    let reviewed = tempfile::tempdir().expect("the test step succeeds");
    let replacement = tempfile::tempdir().expect("the test step succeeds");
    let snapshot = block_on(async {
        let input = StartCaptured {
            cwd: replacement.path().to_owned(),
            cwd_directory: held(reviewed.path()),
            ..run("touch ran", LONG)
        };
        executions()
            .start_captured(input, &CancellationToken::new())
            .await
            .expect("the test step succeeds")
    });
    assert!(!replacement.path().join("ran").exists());
    if cfg!(target_os = "linux") {
        assert_eq!(
            snapshot.state,
            SnapshotState::Completed(CommandStatus::ExitCode(0))
        );
        assert!(reviewed.path().join("ran").exists());
    } else {
        assert_eq!(snapshot.state, SnapshotState::Lost);
        assert_eq!(snapshot.error_name, Some("CommandAuthorityContextMismatch"));
        assert!(!reviewed.path().join("ran").exists());
    }
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

fn a_forced_stop_kills_a_direct_command_that_left_the_session() {
    let escaped = EscapedCommand::start(NO_SUPERVISOR_DEADLINE, LEFT_SESSION);
    let pid = escaped.await_pid();
    kill_process(escaped.supervisor(), Signal::USR1).expect("the test step succeeds");
    assert_killed(pid, "a forced stop");
}

fn losing_the_owner_kills_a_direct_command_that_left_the_session() {
    let mut escaped = EscapedCommand::start(NO_SUPERVISOR_DEADLINE, LEFT_SESSION);
    let pid = escaped.await_pid();
    drop(escaped.owner.take());
    assert_killed(pid, "the loss of its owner");
}

fn a_passed_deadline_kills_a_direct_command_that_left_the_session() {
    let escaped = EscapedCommand::start(SUPERVISOR_DEADLINE_MILLISECONDS, LEFT_SESSION);
    let pid = escaped.await_pid();
    assert_killed(pid, "its deadline");
}

fn a_forced_stop_kills_a_descendant_whose_main_thread_exited() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let escaped = EscapedCommand::start(NO_SUPERVISOR_DEADLINE, MAIN_THREAD_EXITED);
    let pid = escaped.await_pid();
    kill_process(escaped.supervisor(), Signal::USR1).expect("the test step succeeds");
    assert_killed(pid, "a forced stop");
}

fn a_graceful_stop_kills_a_descendant_whose_main_thread_exited() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let escaped = EscapedCommand::start(NO_SUPERVISOR_DEADLINE, MAIN_THREAD_EXITED);
    let pid = escaped.await_pid();
    kill_process(escaped.supervisor(), Signal::TERM).expect("the test step succeeds");
    assert_killed(pid, "a graceful stop");
}

fn losing_the_owner_kills_a_descendant_whose_main_thread_exited() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let mut escaped = EscapedCommand::start(NO_SUPERVISOR_DEADLINE, MAIN_THREAD_EXITED);
    let pid = escaped.await_pid();
    drop(escaped.owner.take());
    assert_killed(pid, "the loss of its owner");
}

fn forcing_a_graceful_stop_after_the_command_exited_kills_what_ignores_sigterm() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let mut settling = EscapedCommand::start(NO_SUPERVISOR_DEADLINE, IGNORES_SIGTERM);
    let pid = settling.await_pid();
    kill_process(settling.supervisor(), Signal::TERM).expect("the test step succeeds");
    settling.await_status();
    let begun = Instant::now();
    kill_process(settling.supervisor(), Signal::USR1).expect("the test step succeeds");
    assert_killed(pid, "a forced stop");
    assert!(begun.elapsed() < PROMPT, "{:?}", begun.elapsed());
}

fn losing_the_owner_after_the_command_exited_kills_what_ignores_sigterm() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let mut settling = EscapedCommand::start(NO_SUPERVISOR_DEADLINE, IGNORES_SIGTERM);
    let pid = settling.await_pid();
    kill_process(settling.supervisor(), Signal::TERM).expect("the test step succeeds");
    settling.await_status();
    let begun = Instant::now();
    drop(settling.owner.take());
    assert_killed(pid, "the loss of its owner");
    assert!(begun.elapsed() < PROMPT, "{:?}", begun.elapsed());
}

fn a_forced_stop_through_another_namespaces_proc_spares_unrelated_processes() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let directory = tempfile::tempdir().expect("the test step succeeds");
    let helper = directory.path().join("foreign_proc.py");
    fs::write(&helper, FOREIGN_PROC).expect("the test step succeeds");
    let report = process::Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--pid",
            "--fork",
            "--mount-proc",
        ])
        .args(["--kill-child", "python3"])
        .arg(&helper)
        .arg("outer")
        .arg(env::current_exe().expect("the test step succeeds"))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    let Some(killed) = report.lines().find_map(|line| line.strip_prefix("killed=")) else {
        println!("note: no user and pid namespaces here, so a foreign /proc went untested");
        return;
    };
    assert!(report.contains("arranged=yes"), "{report}");
    assert!(
        report.contains("target=Z") || report.contains("target=gone"),
        "{report}"
    );
    assert_eq!(killed, "[]", "a forced stop killed unrelated processes");
}

struct EscapedCommand {
    directory: tempfile::TempDir,
    supervisor: process::Child,
    owner: Option<process::ChildStdin>,
}

impl EscapedCommand {
    fn start(deadline: &str, body: &str) -> Self {
        let directory = tempfile::tempdir().expect("the test step succeeds");
        let script = directory.path().join("escape.py");
        fs::write(&script, body).expect("the test step succeeds");
        let mut supervisor =
            process::Command::new(env::current_exe().expect("the test step succeeds"))
                .args([
                    SUPERVISOR_TOKEN,
                    deadline,
                    &supervisor_identity(),
                    "python3",
                ])
                .arg(&script)
                .arg(directory.path().join(ESCAPED_PID))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("the test step succeeds");
        let owner = supervisor.stdin.take();
        let mut escaped = Self {
            directory,
            supervisor,
            owner,
        };
        escaped
            .owner
            .as_mut()
            .expect("the test step succeeds")
            .write_all(SUPERVISOR_RELEASE)
            .expect("the test step succeeds");
        escaped
    }

    fn supervisor(&self) -> Pid {
        Pid::from_child(&self.supervisor)
    }

    fn recorded_pid(&self) -> Option<Pid> {
        fs::read_to_string(self.directory.path().join(ESCAPED_PID))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .and_then(Pid::from_raw)
    }

    fn await_pid(&self) -> Pid {
        let begun = Instant::now();
        loop {
            if let Some(pid) = self.recorded_pid() {
                return pid;
            }
            assert!(begun.elapsed() < LONG, "the command never recorded its pid");
            thread::sleep(POLL);
        }
    }

    fn await_status(&mut self) {
        let mut stderr = self
            .supervisor
            .stderr
            .take()
            .expect("the test step succeeds");
        let (sender, reported) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut seen = Vec::new();
            let mut buffer = [0; 256];
            while let Ok(length @ 1..) = stderr.read(&mut buffer) {
                seen.extend_from_slice(&buffer[..length]);
                let framed = seen
                    .windows(STATUS_FRAME.len())
                    .position(|window| window == STATUS_FRAME)
                    .is_some_and(|start| seen[start..].contains(&b'\n'));
                if framed {
                    let _ = sender.send(());
                }
            }
        });
        reported
            .recv_timeout(LONG)
            .expect("the supervisor reported the command's status");
    }
}

impl Drop for EscapedCommand {
    fn drop(&mut self) {
        let marker = self.directory.path().display().to_string();
        if let Some(pid) = self.recorded_pid()
            && command_line(pid).contains(&marker)
        {
            let _ = kill_process(pid, Signal::KILL);
        }
        let _ = kill_process_group(self.supervisor(), Signal::KILL);
        let _ = self.supervisor.kill();
        let _ = self.supervisor.wait();
    }
}

fn assert_killed(pid: Pid, stop: &str) {
    let begun = Instant::now();
    while alive(pid) {
        assert!(
            begun.elapsed() < LONG,
            "the command that left the session outlived {stop}"
        );
        thread::sleep(POLL);
    }
}

fn alive(pid: Pid) -> bool {
    if cfg!(target_os = "linux") {
        return tasks(pid).any(|task| {
            fs::read_to_string(task.join("stat")).is_ok_and(|stat| {
                stat.rsplit_once(')')
                    .is_some_and(|(_, fields)| !fields.trim_start().starts_with('Z'))
            })
        });
    }
    let state = ps(pid, "stat=");
    !state.is_empty() && !state.starts_with('Z')
}

fn command_line(pid: Pid) -> String {
    if cfg!(target_os = "linux") {
        return tasks(pid)
            .filter_map(|task| fs::read(task.join("cmdline")).ok())
            .map(|line| String::from_utf8_lossy(&line).into_owned())
            .collect();
    }
    ps(pid, "args=")
}

fn tasks(pid: Pid) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(format!("/proc/{pid}/task"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|task| task.path())
}

fn ps(pid: Pid, field: &str) -> String {
    process::Command::new("ps")
        .args(["-o", field, "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn abandoned_starts_release_their_slot_once_they_settle() {
    let directory = tempfile::tempdir().expect("the test step succeeds");
    let ready = fifo(directory.path(), "ready");
    let started = read_fifo_in_background(ready.clone());
    let command = format!("printf up > {}; exec sleep 60", ready.display());
    block_on(async {
        let executions = executions();
        let cancel = CancellationToken::new();
        let mut running = Box::pin(executions.start_captured(run(&command, LONG), &cancel));
        assert!(poll_once(&mut running).await.is_none());
        let text = tokio::task::spawn_blocking(move || started.recv_timeout(LONG))
            .await
            .expect("the test step succeeds");
        assert_eq!(text.as_deref(), Ok("up"));
        drop(running);
        let mut settled = Box::pin(executions.start_captured(run("true", LONG), &cancel));
        assert!(poll_once(&mut settled).await.is_none());
        executions.shutdown().await;
        assert_eq!(
            executions.command("shell-1"),
            None,
            "a start dropped while its command ran kept its slot"
        );
        assert_eq!(executions.command("shell-2").as_deref(), Some("true"));
        drop(settled);
        assert_eq!(
            executions.command("shell-2"),
            None,
            "a start dropped after its command settled kept its slot"
        );
    });
}

fn a_detached_daemon_holding_the_output_ends_the_drain_with_complete_output() {
    let directory = tempfile::tempdir().expect("the test step succeeds");
    let pid_path = directory.path().join("daemon.pid");
    let script = directory.path().join("daemon.py");
    fs::write(
        &script,
        format!(
            "import os, time\n\
             ready_r, ready_w = os.pipe()\n\
             if os.fork() == 0:\n\
             \x20   os.close(ready_r)\n\
             \x20   os.setsid()\n\
             \x20   with open({pid:?}, 'w') as f: f.write(str(os.getpid()))\n\
             \x20   os.write(ready_w, b'R'); os.close(ready_w)\n\
             \x20   time.sleep(30)\n\
             \x20   os._exit(0)\n\
             os.close(ready_w)\n\
             if os.read(ready_r, 1) != b'R': raise SystemExit(1)\n\
             print('BEFORE-EXIT', flush=True)\n",
            pid = pid_path.display().to_string(),
        ),
    )
    .expect("the test step succeeds");
    let begun = Instant::now();
    let snapshot = block_on(async {
        executions()
            .start_captured(
                run(&format!("python3 {}", script.display()), LONG),
                &CancellationToken::new(),
            )
            .await
            .expect("the test step succeeds")
    });
    let elapsed = begun.elapsed();
    let daemon = fs::read_to_string(&pid_path).expect("the test step succeeds");
    let _ = process::Command::new("kill").arg(daemon.trim()).status();
    assert_eq!(
        snapshot.state,
        SnapshotState::Completed(CommandStatus::ExitCode(0))
    );
    assert_eq!(text(&snapshot), "BEFORE-EXIT\n");
    assert!(!snapshot.output_incomplete);
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
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
