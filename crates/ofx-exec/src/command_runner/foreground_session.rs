use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::process::{Signal, kill_process_group, setsid};
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2};

use super::{error_name, launch_failure_prefix, status_prefix};

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
const OWNER_READ_BYTES: usize = 64;
const SURVIVED_SIGNALS: [i32; 5] = [SIGINT, SIGHUP, SIGQUIT, SIGUSR1, SIGUSR2];

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
    let group = setsid().map_err(|_| Failure::Setup)?;
    let terminated = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGTERM, Arc::clone(&terminated)).map_err(|_| Failure::Setup)?;
    for signal in SURVIVED_SIGNALS {
        signal_hook::flag::register(signal, Arc::default()).map_err(|_| Failure::Setup)?;
    }
    io::stderr()
        .write_all(&[READY_BYTE])
        .map_err(|_| Failure::Setup)?;
    let nonce = read_release()?;
    let launch = |error: &io::Error| Failure::Launch {
        nonce: nonce.clone(),
        name: error_name(error),
    };
    thread::Builder::new()
        .spawn(move || {
            watch_owner(deadline);
            let _ = kill_process_group(group, Signal::KILL);
        })
        .map_err(|error| launch(&error))?;
    let mut target = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|error| launch(&error))?;
    if terminated.load(Ordering::SeqCst) {
        let _ = kill_process_group(group, Signal::TERM);
    }
    let status = target.wait().map_err(|error| launch(&error))?;
    report_status(&nonce, status);
    let _ = kill_process_group(group, Signal::KILL);
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

fn watch_owner(deadline: Option<Instant>) {
    let stdin = io::stdin();
    let mut buffer = [0; OWNER_READ_BYTES];
    loop {
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if remaining == Some(Duration::ZERO) {
            return;
        }
        let timeout = remaining.and_then(|remaining| Timespec::try_from(remaining).ok());
        let mut descriptors = [PollFd::new(&stdin, PollFlags::IN)];
        match poll(&mut descriptors, timeout.as_ref()) {
            Ok(0) | Err(Errno::INTR) => {}
            Ok(_) => match rustix::io::read(&stdin, &mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            },
            Err(_) => return,
        }
    }
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
