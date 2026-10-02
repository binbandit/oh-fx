mod group_tree;
mod supervision;

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group, setsid};

use super::{error_name, launch_failure_prefix, status_prefix};
use group_tree::GroupTree;
pub(super) use supervision::FORCE_SIGNAL;
use supervision::{Requests, Supervision};

pub(super) const TOKEN: &str = "__oh_fx_foreground_session__";
pub(super) const READY_BYTE: u8 = 0x1e;
pub(super) const RELEASE_BYTE: u8 = 0x06;
pub(super) const NONCE_HEX_BYTES: usize = 32;
pub(super) const LAUNCH_FAILURE_EXIT_CODE: i32 = 125;
pub(super) const LAUNCH_FAILURE_PREFIX: &[u8] = b"\0OH_FX_FOREGROUND_EXEC_FAILED:";
pub(super) const STATUS_PREFIX: &[u8] = b"\0OH_FX_FOREGROUND_STATUS:";
pub(super) const EXIT_STATUS: &str = "exit:";
pub(super) const SIGNAL_STATUS: &str = "signal:";
pub(super) const NO_DEADLINE: &str = "none";
const SETUP_FAILURE_EXIT_CODE: i32 = 1;
const UNKNOWN_TERMINATION_EXIT_CODE: i32 = 127;

enum Failure {
    Setup,
    Launch { nonce: String, name: &'static str },
}

pub fn is_foreground_session_invocation(args: &[OsString]) -> bool {
    args.first().is_some_and(|argument| argument == TOKEN)
}

pub fn run_foreground_session(args: &[OsString]) -> ! {
    let code = match supervise(args) {
        Ok(status) => exit_code(status),
        Err(Failure::Setup) => SETUP_FAILURE_EXIT_CODE,
        Err(Failure::Launch { nonce, name }) => {
            let mut marker = launch_failure_prefix(&nonce);
            marker.extend_from_slice(name.as_bytes());
            marker.push(b'\n');
            let _ = io::stderr().write_all(&marker);
            LAUNCH_FAILURE_EXIT_CODE
        }
    };
    std::process::exit(code)
}

fn supervise(args: &[OsString]) -> Result<ExitStatus, Failure> {
    let [token, deadline, program, arguments @ ..] = args else {
        return Err(Failure::Setup);
    };
    if token != TOKEN {
        return Err(Failure::Setup);
    }
    let deadline = parse_deadline(deadline)?;
    let session = setsid().map_err(|_| Failure::Setup)?;
    let requests = Requests::register().map_err(|_| Failure::Setup)?;
    io::stderr()
        .write_all(&[READY_BYTE])
        .map_err(|_| Failure::Setup)?;
    let nonce = read_release()?;
    let launch = |name: &'static str| Failure::Launch {
        nonce: nonce.clone(),
        name,
    };
    let target = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|error| launch(error_name(&error)))?;
    let target = Pid::from_child(&target);
    if requests.graceful_requested() {
        let _ = kill_process_group(session, Signal::TERM);
    }
    let mut supervision = Supervision::new(target, deadline, requests, GroupTree::new(session));
    let status = supervision.wait_for_target().map_err(|name| {
        supervision.kill_target();
        launch(name)
    })?;
    report_status(&nonce, status);
    supervision.settle().map_err(launch)?;
    let _ = kill_process_group(session, Signal::KILL);
    Ok(status)
}

fn report_status(nonce: &str, status: ExitStatus) {
    let reported = match (status.code(), status.signal()) {
        (Some(code), _) => format!("{EXIT_STATUS}{code}"),
        (None, Some(signal)) => format!("{SIGNAL_STATUS}{signal}"),
        (None, None) => return,
    };
    let mut frame = status_prefix(nonce);
    frame.extend_from_slice(reported.as_bytes());
    frame.push(b'\n');
    let _ = io::stderr().write_all(&frame);
}

fn parse_deadline(raw: &OsString) -> Result<Option<Instant>, Failure> {
    let raw = raw.to_str().ok_or(Failure::Setup)?;
    if raw == NO_DEADLINE {
        return Ok(None);
    }
    let milliseconds = raw.parse::<u64>().map_err(|_| Failure::Setup)?;
    Ok(Instant::now().checked_add(Duration::from_millis(milliseconds)))
}

fn read_release() -> Result<String, Failure> {
    let mut control = [0; NONCE_HEX_BYTES + 1];
    io::stdin()
        .lock()
        .read_exact(&mut control)
        .map_err(|_| Failure::Setup)?;
    let (nonce, release) = control.split_at(NONCE_HEX_BYTES);
    if release != [RELEASE_BYTE] || !nonce.iter().all(u8::is_ascii_hexdigit) {
        return Err(Failure::Setup);
    }
    String::from_utf8(nonce.to_vec()).map_err(|_| Failure::Setup)
}

fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    match status.signal() {
        Some(signal) => {
            let _ = signal_hook::low_level::emulate_default_handler(signal);
            128 + signal
        }
        None => UNKNOWN_TERMINATION_EXIT_CODE,
    }
}
