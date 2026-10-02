use std::env;
use std::ffi::OsString;
use std::fs;
use std::panic;
use std::path::PathBuf;
use std::process::{self, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ofx_exec::{
    CommandStatus, Environment, HeldDirectory, ManagedExecutions, SessionSupervisor, Snapshot,
    SnapshotState, StartCaptured, is_foreground_session_invocation, run_foreground_session,
};
use rustix::process::{Pid, Signal, kill_process};
use tokio_util::sync::CancellationToken;

const OWNER_COMMAND_VARIABLE: &str = "OH_FX_TEST_PROCESS_TREE_OWNER_COMMAND";
const BASH: &str = "/bin/bash";
const LONG: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(10);
const DEADLINE: Duration = Duration::from_secs(3);
const TRACKS_DESCENDANTS: bool = cfg!(target_os = "linux");
const PRELUDE: &str = r"
import os, signal, sys, time

def path(name):
    return os.path.join(DIRECTORY, name)

def publish(name, text):
    with open(path(name) + '.tmp', 'w') as staged:
        staged.write(text)
    os.rename(path(name) + '.tmp', path(name))

def record(name, pid=None):
    descriptor = os.open(path(name), os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    os.write(descriptor, (str(pid or os.getpid()) + '\n').encode())
    os.close(descriptor)

def silence():
    null = os.open('/dev/null', os.O_RDWR)
    for descriptor in (0, 1, 2):
        os.dup2(null, descriptor)

def daemon(name, double_fork=False, worker=False, wait=True, setup=None):
    ready_r, ready_w = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(ready_r)
        os.setsid()
        if double_fork and os.fork() > 0:
            os._exit(0)
        silence()
        if setup:
            setup()
        if worker and os.fork() == 0:
            record(name)
            time.sleep(60)
            os._exit(0)
        record(name)
        if wait:
            os.write(ready_w, b'R')
        os.close(ready_w)
        time.sleep(60)
        os._exit(0)
    os.close(ready_w)
    if wait and os.read(ready_r, 1) != b'R':
        sys.exit(1)
    os.close(ready_r)
    if double_fork:
        os.waitpid(pid, 0)

def churn(rounds):
    for _ in range(rounds):
        pid = os.fork()
        if pid == 0:
            if os.fork() == 0:
                os._exit(0)
            os._exit(0)
        os.waitpid(pid, 0)
";

type Test = fn();

macro_rules! tests {
    ($($name:ident),* $(,)?) => {
        &[$((stringify!($name), $name as Test)),*]
    };
}

const PORTABLE: &[(&str, Test)] = tests![
    natural_command_completion_terminates_background_child_inheriting_pipes,
    natural_command_completion_keeps_a_daemon_that_detached_after_setsid,
    natural_command_completion_keeps_a_double_forked_daemon_and_its_children,
    natural_command_completion_keeps_a_detached_daemon_that_hides_its_descriptors,
    natural_completion_after_fork_churn_keeps_only_detached_daemons,
];

const TRACKING: &[(&str, Test)] = tests![
    timeout_terminates_redirected_descendant_after_setsid,
    timeout_terminates_double_forked_descendant_after_setsid,
    cancellation_preserves_grace_and_removes_an_escaped_descendant,
    a_forced_stop_removes_a_double_forked_daemon,
    a_graceful_stop_reaches_a_process_that_left_the_process_group,
    a_graceful_stop_reaches_the_children_of_an_adopted_orphan,
    losing_the_owner_stops_a_double_forked_daemon,
    stopping_during_fork_churn_removes_every_detached_daemon,
    a_deadline_during_fork_churn_removes_every_detached_daemon,
    natural_command_completion_stops_a_same_session_process_that_left_the_process_group,
    natural_command_completion_stops_an_attached_process_whose_main_thread_exited,
    orphans_are_reaped_without_taking_the_command_status,
];

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if is_foreground_session_invocation(&args) {
        run_foreground_session(&args);
    }
    if let Some(command) = env::var_os(OWNER_COMMAND_VARIABLE) {
        owner_child(&command.to_string_lossy());
    }
    let filters: Vec<String> = args
        .iter()
        .filter_map(|argument| argument.to_str())
        .filter(|argument| !argument.starts_with('-'))
        .map(str::to_owned)
        .collect();
    let mut failed = Vec::new();
    let selected = PORTABLE
        .iter()
        .map(|&test| (test, true))
        .chain(TRACKING.iter().map(|&test| (test, TRACKS_DESCENDANTS)));
    for ((name, test), supported) in selected {
        if !filters.is_empty() && !filters.iter().any(|filter| name.contains(filter.as_str())) {
            continue;
        }
        if !supported {
            println!("test {name} ... ignored");
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

struct Scratch {
    directory: tempfile::TempDir,
}

impl Scratch {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().expect("create a scratch directory"),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn python(&self, body: &str) -> String {
        let script = self.path("command.py");
        let directory = format!("{:?}", self.directory.path().display().to_string());
        fs::write(
            &script,
            format!("DIRECTORY = {directory}\n{PRELUDE}\n{body}\n"),
        )
        .expect("write the command script");
        format!("exec python3 {}", script.display())
    }

    fn pids(&self, name: &str) -> Vec<Pid> {
        fs::read_to_string(self.path(name))
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(parse_pid)
            .collect()
    }

    fn await_pids(&self, name: &str, count: usize) -> Vec<Pid> {
        let started = Instant::now();
        loop {
            let pids = self.pids(name);
            if pids.len() >= count {
                return pids;
            }
            assert!(
                started.elapsed() < LONG,
                "only {} of {count} {name} pids were recorded",
                pids.len()
            );
            thread::sleep(POLL);
        }
    }

    fn await_file(&self, name: &str) -> String {
        let started = Instant::now();
        loop {
            if let Ok(text) = fs::read_to_string(self.path(name)) {
                return text;
            }
            assert!(started.elapsed() < LONG, "{name} was never published");
            thread::sleep(POLL);
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let marker = self.directory.path().display().to_string();
        let recorded = fs::read_dir(self.directory.path())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| fs::read_to_string(entry.path()).ok())
            .filter_map(|text| {
                text.split_whitespace()
                    .map(parse_pid)
                    .collect::<Option<Vec<Pid>>>()
            })
            .flatten();
        for pid in recorded {
            if command_line(pid).contains(&marker) {
                let _ = kill_process(pid, Signal::KILL);
            }
        }
    }
}

fn parse_pid(text: &str) -> Option<Pid> {
    text.parse::<u32>()
        .ok()
        .and_then(|raw| i32::try_from(raw).ok())
        .and_then(Pid::from_raw)
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

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a runtime")
        .block_on(future)
}

fn executions() -> ManagedExecutions {
    ManagedExecutions::new(SessionSupervisor::new(
        env::current_exe().expect("the test binary"),
    ))
}

fn run(command: &str, yield_time: Duration) -> StartCaptured {
    StartCaptured {
        command: command.to_owned(),
        cwd: env::temp_dir(),
        cwd_directory: HeldDirectory::new(
            fs::File::open(env::temp_dir())
                .expect("the temporary directory")
                .into(),
        ),
        environment: Environment::Clean(BASH.into()),
        max_output_bytes: 64 * 1024,
        timeout: None,
        yield_time,
    }
}

fn complete(command: &str) -> Snapshot {
    block_on(async {
        executions()
            .start_captured(run(command, LONG), &CancellationToken::new())
            .await
            .expect("start the command")
    })
}

fn complete_before_deadline(command: &str) -> Snapshot {
    let input = StartCaptured {
        timeout: Some(DEADLINE),
        ..run(command, LONG)
    };
    block_on(async {
        executions()
            .start_captured(input, &CancellationToken::new())
            .await
            .expect("start the command")
    })
}

fn stop_once(scratch: &Scratch, command: &str, ready: &str, force: bool) -> (Snapshot, Duration) {
    block_on(async {
        let executions = executions();
        let started = executions
            .start_captured(run(command, Duration::ZERO), &CancellationToken::new())
            .await
            .expect("start the command");
        assert_eq!(started.state, SnapshotState::Running);
        let path = scratch.path(ready);
        tokio::task::spawn_blocking(move || {
            let begun = Instant::now();
            while !path.exists() {
                assert!(begun.elapsed() < LONG, "the command never became ready");
                thread::sleep(POLL);
            }
        })
        .await
        .expect("wait for readiness");
        let begun = Instant::now();
        let stopped = executions
            .stop(&started.execution_id, force)
            .await
            .expect("stop the command");
        (stopped, begun.elapsed())
    })
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

fn ps(pid: Pid, field: &str) -> String {
    process::Command::new("ps")
        .args(["-o", field, "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn assert_gone(pids: &[Pid]) {
    let started = Instant::now();
    while let Some(survivor) = pids.iter().find(|&&pid| alive(pid)) {
        assert!(started.elapsed() < LONG, "{survivor} outlived its command");
        thread::sleep(POLL);
    }
}

fn assert_alive(pids: &[Pid]) {
    for &pid in pids {
        assert!(alive(pid), "{pid} was stopped with its command");
    }
}

fn timeout_terminates_redirected_descendant_after_setsid() {
    let scratch = Scratch::new();
    let command = scratch.python("daemon('daemon')\nwhile True: time.sleep(1)");
    let snapshot = complete_before_deadline(&command);
    assert_eq!(snapshot.error_name, Some("TimeoutExpired"));
    assert_gone(&scratch.await_pids("daemon", 1));
}

fn timeout_terminates_double_forked_descendant_after_setsid() {
    let scratch = Scratch::new();
    let command = scratch.python("daemon('daemon', double_fork=True)\nwhile True: time.sleep(1)");
    let snapshot = complete_before_deadline(&command);
    assert_eq!(snapshot.error_name, Some("TimeoutExpired"));
    assert_gone(&scratch.await_pids("daemon", 1));
}

fn cancellation_preserves_grace_and_removes_an_escaped_descendant() {
    let scratch = Scratch::new();
    let command = scratch.python(
        "def hold(signum, frame):\n\
         \x20   publish('term', 'TERM')\n\
         \x20   time.sleep(3)\n\
         \x20   os._exit(130)\n\
         daemon('daemon', setup=lambda: signal.signal(signal.SIGTERM, hold))\n\
         publish('ready', 'R')\n\
         while True: time.sleep(1)",
    );
    let (stopped, elapsed) = stop_once(&scratch, &command, "ready", false);
    assert_eq!(
        stopped.state,
        SnapshotState::Stopped(Some(CommandStatus::Signal(15)))
    );
    assert!(elapsed >= Duration::from_millis(500), "{elapsed:?}");
    assert_eq!(scratch.await_file("term"), "TERM");
    assert_gone(&scratch.await_pids("daemon", 1));
}

fn a_forced_stop_removes_a_double_forked_daemon() {
    let scratch = Scratch::new();
    let command = scratch.python(
        "daemon('daemon', double_fork=True)\npublish('ready', 'R')\nwhile True: time.sleep(1)",
    );
    let (stopped, _) = stop_once(&scratch, &command, "ready", true);
    assert_eq!(
        stopped.state,
        SnapshotState::Stopped(Some(CommandStatus::Signal(9)))
    );
    assert_gone(&scratch.await_pids("daemon", 1));
}

fn a_graceful_stop_reaches_a_process_that_left_the_process_group() {
    let scratch = Scratch::new();
    let command = scratch.python(
        "def finish(signum, frame):\n\
         \x20   publish('term', 'TERM')\n\
         \x20   os._exit(0)\n\
         pid = os.fork()\n\
         if pid == 0:\n\
         \x20   os.setpgid(0, 0)\n\
         \x20   signal.signal(signal.SIGTERM, finish)\n\
         \x20   silence()\n\
         \x20   record('moved')\n\
         \x20   publish('ready', 'R')\n\
         \x20   while True: time.sleep(1)\n\
         while True: time.sleep(1)",
    );
    let (stopped, _) = stop_once(&scratch, &command, "ready", false);
    assert_eq!(
        stopped.state,
        SnapshotState::Stopped(Some(CommandStatus::Signal(15)))
    );
    assert_eq!(scratch.await_file("term"), "TERM");
    assert_gone(&scratch.await_pids("moved", 1));
}

fn a_graceful_stop_reaches_the_children_of_an_adopted_orphan() {
    let scratch = Scratch::new();
    let command = scratch.python(
        "def finish(signum, frame):\n\
         \x20   publish('term', 'TERM')\n\
         \x20   os._exit(0)\n\
         middle = os.fork()\n\
         if middle == 0:\n\
         \x20   middle_pid = os.getpid()\n\
         \x20   if os.fork() == 0:\n\
         \x20       signal.signal(signal.SIGTERM, signal.SIG_IGN)\n\
         \x20       silence()\n\
         \x20       while os.getppid() == middle_pid: time.sleep(0.001)\n\
         \x20       record('adopted')\n\
         \x20       if os.fork() == 0:\n\
         \x20           os.setsid()\n\
         \x20           signal.signal(signal.SIGTERM, finish)\n\
         \x20           record('escaped')\n\
         \x20           publish('ready', 'R')\n\
         \x20           while True: time.sleep(1)\n\
         \x20       while True: time.sleep(1)\n\
         \x20   os._exit(0)\n\
         os.waitpid(middle, 0)\n\
         while True: time.sleep(1)",
    );
    let (stopped, _) = stop_once(&scratch, &command, "ready", false);
    assert_eq!(
        stopped.state,
        SnapshotState::Stopped(Some(CommandStatus::Signal(15)))
    );
    assert_eq!(scratch.await_file("term"), "TERM");
    assert_gone(&scratch.await_pids("escaped", 1));
    assert_gone(&scratch.await_pids("adopted", 1));
}

fn losing_the_owner_stops_a_double_forked_daemon() {
    let scratch = Scratch::new();
    let command = scratch.python(
        "record('started')\n\
         record('started', os.getsid(0))\n\
         daemon('daemon', double_fork=True)\n\
         while True: time.sleep(1)",
    );
    let owner = Owner(
        process::Command::new(env::current_exe().expect("the test binary"))
            .env(OWNER_COMMAND_VARIABLE, &command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("start the owner"),
    );
    let daemons = scratch.await_pids("daemon", 1);
    drop(owner);
    assert_gone(&daemons);
}

struct Owner(process::Child);

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn stopping_during_fork_churn_removes_every_detached_daemon() {
    let scratch = Scratch::new();
    let command = scratch.python(&churning_daemons(ESCAPE_ON_SIGTERM));
    let (stopped, _) = stop_once(&scratch, &command, "ready", false);
    assert!(
        matches!(stopped.state, SnapshotState::Stopped(Some(_))),
        "{stopped:?}"
    );
    let daemons = scratch.await_pids("daemons", 16);
    let escaped = scratch.await_pids("escaped", 1);
    let late = scratch.pids("late");
    assert_gone(&daemons);
    assert_gone(&escaped);
    assert_gone(&late);
}

fn a_deadline_during_fork_churn_removes_every_detached_daemon() {
    let scratch = Scratch::new();
    let command = scratch.python(&churning_daemons(""));
    let snapshot = complete_before_deadline(&command);
    assert_eq!(snapshot.error_name, Some("TimeoutExpired"));
    let daemons = scratch.await_pids("daemons", 16);
    let late = scratch.await_pids("late", 1);
    assert_gone(&daemons);
    assert_gone(&late);
}

const ESCAPE_ON_SIGTERM: &str = "command = os.getpid()\n\
     def escape(signum, frame):\n\
     \x20   if os.getpid() == command:\n\
     \x20       daemon('escaped', double_fork=True, setup=lambda: signal.signal(signal.SIGTERM, signal.SIG_IGN))\n\
     \x20   os._exit(0)\n\
     signal.signal(signal.SIGTERM, escape)\n";

fn churning_daemons(before_ready: &str) -> String {
    format!(
        "for index in range(16):\n\
         \x20   daemon('daemons', double_fork=index % 2 == 0)\n\
         \x20   churn(4)\n\
         {before_ready}\
         publish('ready', 'R')\n\
         rounds = 0\n\
         while True:\n\
         \x20   if rounds % 16 == 0:\n\
         \x20       daemon('late', double_fork=True, wait=False)\n\
         \x20   churn(8)\n\
         \x20   rounds += 1\n\
         \x20   time.sleep(0.001)"
    )
}

fn natural_command_completion_terminates_background_child_inheriting_pipes() {
    let scratch = Scratch::new();
    let pid_path = scratch.path("child");
    let snapshot = complete(&format!(
        "(exec -a {} sleep 30) & printf '%s' $! > {}",
        scratch.path("sleeper").display(),
        pid_path.display()
    ));
    assert_eq!(
        snapshot.state,
        SnapshotState::Completed(CommandStatus::ExitCode(0))
    );
    let child = scratch.await_pids("child", 1);
    assert_gone(&child);
}

fn natural_command_completion_keeps_a_daemon_that_detached_after_setsid() {
    let scratch = Scratch::new();
    let snapshot = complete(&scratch.python("daemon('daemon')"));
    assert_eq!(
        snapshot.state,
        SnapshotState::Completed(CommandStatus::ExitCode(0))
    );
    assert_alive(&scratch.await_pids("daemon", 1));
}

fn natural_command_completion_keeps_a_double_forked_daemon_and_its_children() {
    let scratch = Scratch::new();
    let snapshot = complete(
        &scratch.python("daemon('daemon', double_fork=True, worker=True)\nprint('DAEMON-STARTED')"),
    );
    assert_eq!(
        snapshot.state,
        SnapshotState::Completed(CommandStatus::ExitCode(0))
    );
    assert!(String::from_utf8_lossy(&snapshot.output_delta).contains("DAEMON-STARTED"));
    assert_alive(&scratch.await_pids("daemon", 2));
}

fn natural_command_completion_keeps_a_detached_daemon_that_hides_its_descriptors() {
    let scratch = Scratch::new();
    let command = scratch.python(
        "import ctypes\n\
         def hide():\n\
         \x20   if sys.platform.startswith('linux'):\n\
         \x20       ctypes.CDLL(None).prctl(4, 0, 0, 0, 0)\n\
         ready_r, ready_w = os.pipe()\n\
         if os.fork() == 0:\n\
         \x20   os.close(ready_r)\n\
         \x20   os.setsid()\n\
         \x20   hide()\n\
         \x20   record('daemon')\n\
         \x20   os.write(ready_w, b'R')\n\
         \x20   os.closerange(0, 65536)\n\
         \x20   time.sleep(60)\n\
         \x20   os._exit(0)\n\
         os.close(ready_w)\n\
         if os.read(ready_r, 1) != b'R': sys.exit(1)\n\
         time.sleep(0.1)",
    );
    let snapshot = complete(&command);
    assert_eq!(
        snapshot.state,
        SnapshotState::Completed(CommandStatus::ExitCode(0))
    );
    assert_alive(&scratch.await_pids("daemon", 1));
}

fn natural_command_completion_stops_a_same_session_process_that_left_the_process_group() {
    let scratch = Scratch::new();
    let command = scratch.python(
        "ready_r, ready_w = os.pipe()\n\
         if os.fork() == 0:\n\
         \x20   os.close(ready_r)\n\
         \x20   os.setpgid(0, 0)\n\
         \x20   silence()\n\
         \x20   record('moved')\n\
         \x20   os.write(ready_w, b'R')\n\
         \x20   time.sleep(60)\n\
         \x20   os._exit(0)\n\
         os.close(ready_w)\n\
         if os.read(ready_r, 1) != b'R': sys.exit(1)",
    );
    let snapshot = complete(&command);
    assert_eq!(
        snapshot.state,
        SnapshotState::Completed(CommandStatus::ExitCode(0))
    );
    let moved = scratch.await_pids("moved", 1);
    assert_gone(&moved);
}

fn natural_command_completion_stops_an_attached_process_whose_main_thread_exited() {
    let scratch = Scratch::new();
    let command = scratch.python(
        "import ctypes, threading\n\
         def state():\n\
         \x20   return open('/proc/%d/stat' % os.getpid()).read().rsplit(')', 1)[1].split()[0]\n\
         def work():\n\
         \x20   while state() != 'Z': time.sleep(0.001)\n\
         \x20   os.write(ready_w, b'R')\n\
         \x20   while True: time.sleep(1)\n\
         ready_r, ready_w = os.pipe()\n\
         if os.fork() == 0:\n\
         \x20   os.close(ready_r)\n\
         \x20   os.setpgid(0, 0)\n\
         \x20   silence()\n\
         \x20   record('worker')\n\
         \x20   threading.Thread(target=work).start()\n\
         \x20   ctypes.CDLL(None).pthread_exit(None)\n\
         os.close(ready_w)\n\
         if os.read(ready_r, 1) != b'R': sys.exit(1)",
    );
    let snapshot = complete(&command);
    assert_eq!(
        snapshot.state,
        SnapshotState::Completed(CommandStatus::ExitCode(0))
    );
    assert_gone(&scratch.await_pids("worker", 1));
}

fn natural_completion_after_fork_churn_keeps_only_detached_daemons() {
    let scratch = Scratch::new();
    let command = scratch.python(
        "for index in range(8):\n\
         \x20   daemon('daemons', double_fork=index % 2 == 0)\n\
         \x20   if os.fork() == 0:\n\
         \x20       silence()\n\
         \x20       record('attached')\n\
         \x20       time.sleep(60)\n\
         \x20       os._exit(0)\n\
         \x20   churn(16)\n\
         while len(open(path('attached')).read().split()) < 8:\n\
         \x20   time.sleep(0.01)",
    );
    let snapshot = complete(&command);
    assert_eq!(
        snapshot.state,
        SnapshotState::Completed(CommandStatus::ExitCode(0))
    );
    let daemons = scratch.await_pids("daemons", 8);
    let attached = scratch.await_pids("attached", 8);
    assert_gone(&attached);
    assert_alive(&daemons);
}

fn orphans_are_reaped_without_taking_the_command_status() {
    let scratch = Scratch::new();
    let command = scratch.python(
        "for index in range(64):\n\
         \x20   pid = os.fork()\n\
         \x20   if pid == 0:\n\
         \x20       if os.fork() == 0:\n\
         \x20           os._exit(3)\n\
         \x20       os._exit(0)\n\
         \x20   os.waitpid(pid, 0)\n\
         supervisor = os.getsid(0)\n\
         def adopted():\n\
         \x20   found = 0\n\
         \x20   for task in os.listdir('/proc/%d/task' % supervisor):\n\
         \x20       for child in open('/proc/%d/task/%s/children' % (supervisor, task)).read().split():\n\
         \x20           if int(child) != os.getpid():\n\
         \x20               found += 1\n\
         \x20   return found\n\
         waited = time.time() + 10\n\
         while adopted() and time.time() < waited:\n\
         \x20   time.sleep(0.01)\n\
         print('adopted', adopted())\n\
         sys.exit(7)",
    );
    let snapshot = complete(&command);
    assert_eq!(
        snapshot.state,
        SnapshotState::Completed(CommandStatus::ExitCode(7))
    );
    assert_eq!(
        String::from_utf8_lossy(&snapshot.output_delta),
        "adopted 0\n"
    );
}

fn owner_child(command: &str) -> ! {
    block_on(async {
        let executions = executions();
        let snapshot = executions
            .start_captured(run(command, Duration::ZERO), &CancellationToken::new())
            .await
            .expect("start the command");
        assert_eq!(snapshot.state, SnapshotState::Running);
        std::future::pending::<()>().await;
    });
    process::exit(1)
}
