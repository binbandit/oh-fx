use std::env;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio::sync::{oneshot, watch};
use tokio::time::{Instant, timeout};

use rustix::process::Signal;

use super::{
    Collection, FORCE_SIGNAL, OutputStream, SessionSupervisor, StopIntent, child_pid,
    termination_signal, watch_exit,
};
use crate::command_contract::CommandStatus;

const NONCE: &str = "0123456789abcdef0123456789abcdef";
const LONG: Duration = Duration::from_secs(30);
const LEFTOVER_BYTES: usize = 64 * 1024;

#[test]
fn the_supervisor_reexecutes_the_live_inode_rather_than_the_on_disk_path() {
    let supervisor = SessionSupervisor::current_executable().expect("the running executable");
    let on_disk = env::current_exe().expect("the on-disk executable");
    if cfg!(target_os = "linux") {
        assert_eq!(supervisor, SessionSupervisor::new("/proc/self/exe"));
        let live = fs::metadata("/proc/self/exe").expect("the live inode");
        let file = fs::metadata(&on_disk).expect("the on-disk file");
        assert_eq!((live.dev(), live.ino()), (file.dev(), file.ino()));
    } else {
        assert_eq!(supervisor, SessionSupervisor::new(on_disk));
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a runtime")
        .block_on(future)
}

fn spawn_group(script: &str) -> (Child, ChildStdout, ChildStderr) {
    let mut child = tokio::process::Command::new("/bin/sh")
        .args(["-c", script])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the command");
    let stdout = child.stdout.take().expect("the command's stdout");
    let stderr = child.stderr.take().expect("the command's stderr");
    (child, stdout, stderr)
}

#[test]
fn settlement_expiry_ends_collection_while_the_exit_is_still_pending() {
    block_on(async {
        let (mut child, stdout, stderr) = spawn_group("exec sleep 60");
        let group = child_pid(&child).expect("the command's group");
        let (_never_sent, exited) = oneshot::channel();
        let started = Instant::now();
        let mut collection =
            Collection::new(&mut child, group, (stdout, stderr), NONCE, None, exited);
        let (requests, mut stop) = watch::channel(None);
        requests.send_replace(Some(StopIntent::Force));
        let mut sink = |_: OutputStream, _: &[u8]| {};
        let exit = timeout(LONG, collection.collect(&mut stop, &mut sink))
            .await
            .expect("collection ends at the settlement deadline");
        assert_eq!(exit, None);
        let outcome = collection
            .finish(exit, started)
            .expect("an indeterminate outcome");
        assert_eq!(outcome.status, CommandStatus::Indeterminate);
        assert!(outcome.output_incomplete);
        let reaped = timeout(LONG, child.wait())
            .await
            .expect("the killed command is reaped");
        assert!(reaped.is_ok());
    });
}

#[test]
fn a_slow_consumer_still_receives_every_leftover_byte_after_exit() {
    block_on(async {
        let (mut child, stdout, stderr) =
            spawn_group(&format!("head -c {LEFTOVER_BYTES} /dev/zero"));
        let group = child_pid(&child).expect("the command's group");
        let exited = watch_exit(group).expect("watch the command's exit");
        let started = Instant::now();
        let mut collection =
            Collection::new(&mut child, group, (stdout, stderr), NONCE, None, exited);
        let (_requests, mut stop) = watch::channel(None);
        let mut received = 0;
        let mut sink = |_: OutputStream, bytes: &[u8]| {
            received += bytes.len();
            std::thread::sleep(Duration::from_millis(200));
        };
        let exit = timeout(LONG, collection.collect(&mut stop, &mut sink))
            .await
            .expect("collection ends once the output is drained");
        let outcome = collection.finish(exit, started).expect("an outcome");
        assert_eq!(received, LEFTOVER_BYTES);
        assert_eq!(outcome.status, CommandStatus::ExitCode(0));
        assert!(!outcome.output_incomplete);
    });
}

#[test]
fn foreground_force_cleanup_preserves_the_supervisor() {
    assert_eq!(FORCE_SIGNAL, Signal::USR1);
    assert_eq!(termination_signal(StopIntent::Force), FORCE_SIGNAL);
    assert_eq!(termination_signal(StopIntent::Graceful), Signal::TERM);
}
