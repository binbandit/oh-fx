use std::path::Path;
use std::process::Command;

#[cfg(target_os = "linux")]
pub(crate) fn exit_peak_rss_kib(dir: &Path, command: Command) -> Result<u64, String> {
    linux::exit_peak_rss_kib(dir, command)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn exit_peak_rss_kib(_dir: &Path, _command: Command) -> Result<u64, String> {
    Err("own peak memory is measured only on Linux".to_owned())
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::path::Path;
    use std::process::{Child, Command};
    use std::thread;
    use std::time::Instant;

    use nix::errno::Errno;
    use nix::sys::ptrace::{self, Event, Options};
    use nix::sys::signal::Signal;
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    use nix::unistd::Pid;

    use crate::measure::{POLL, RUN_TIMEOUT, Spawned, exit_failure, spawn_logged};

    pub(super) fn exit_peak_rss_kib(dir: &Path, command: Command) -> Result<u64, String> {
        let Spawned {
            mut child,
            program,
            stderr_path,
        } = spawn_logged(dir, command)?;
        let raw = i32::try_from(child.id()).map_err(|error| format!("{program} pid: {error}"))?;
        let pid = Pid::from_raw(raw);
        if let Err(error) = ptrace::seize(pid, Options::PTRACE_O_TRACEEXIT) {
            let sampled = sample_until_exit(&mut child, &program, &stderr_path)?;
            return Err(format!(
                "could not trace {program} to read its exit peak ({error}); samples saw at least {sampled} KiB"
            ));
        }
        let deadline = Instant::now() + RUN_TIMEOUT;
        let mut exit_peak = None;
        loop {
            let status = match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(status) => status,
                Err(Errno::EINTR) => continue,
                Err(error) => return Err(format!("wait for {program}: {error}")),
            };
            match status {
                WaitStatus::StillAlive => {
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        reap(pid);
                        return Err(format!("{program} ran longer than {RUN_TIMEOUT:?}"));
                    }
                    thread::sleep(POLL);
                }
                WaitStatus::PtraceEvent(_, _, event) => {
                    if event == Event::PTRACE_EVENT_EXIT as i32 {
                        exit_peak = vm_hwm_kib(child.id());
                    }
                    resume(pid, None, &program)?;
                }
                WaitStatus::Stopped(_, signal) => resume(pid, Some(signal), &program)?,
                WaitStatus::Exited(_, 0) => break,
                WaitStatus::Exited(_, code) => {
                    return Err(exit_failure(
                        &program,
                        &format!("exit status: {code}"),
                        &stderr_path,
                    ));
                }
                WaitStatus::Signaled(_, signal, _) => {
                    return Err(exit_failure(
                        &program,
                        &format!("signal {signal:?}"),
                        &stderr_path,
                    ));
                }
                _ => {}
            }
        }
        exit_peak.ok_or_else(|| format!("{program} exited without a readable exit peak"))
    }

    fn sample_until_exit(
        child: &mut Child,
        program: &str,
        stderr_path: &Path,
    ) -> Result<u64, String> {
        let deadline = Instant::now() + RUN_TIMEOUT;
        let mut sampled = 0;
        loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|error| format!("wait for {program}: {error}"))?
            {
                return if status.success() {
                    Ok(sampled)
                } else {
                    Err(exit_failure(program, &status.to_string(), stderr_path))
                };
            }
            sampled = sampled.max(vm_hwm_kib(child.id()).unwrap_or(0));
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} ran longer than {RUN_TIMEOUT:?}"));
            }
            thread::sleep(POLL);
        }
    }

    fn resume(pid: Pid, signal: Option<Signal>, program: &str) -> Result<(), String> {
        ptrace::cont(pid, signal).map_err(|error| format!("resume {program}: {error}"))
    }

    fn reap(pid: Pid) {
        while let Ok(status) = waitpid(pid, None) {
            if matches!(status, WaitStatus::Exited(..) | WaitStatus::Signaled(..)) {
                return;
            }
            let _ = ptrace::cont(pid, None);
        }
    }

    fn vm_hwm_kib(pid: u32) -> Option<u64> {
        let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        status
            .lines()
            .find_map(|line| line.strip_prefix("VmHWM:"))
            .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const LATE_ALLOCATION_KIB: u64 = 64 * 1024;

        #[test]
        fn the_exit_peak_covers_an_allocation_made_just_before_exit() {
            let dir = tempfile::tempdir().expect("a scratch directory");
            let mut command = Command::new("/bin/sh");
            command.args([
                "-c",
                "sleep 0.05; exec dd if=/dev/zero of=/dev/null bs=64M count=1 2>/dev/null",
            ]);
            let peak = exit_peak_rss_kib(dir.path(), command).expect("a traced run");
            assert!(
                peak >= LATE_ALLOCATION_KIB,
                "an exit peak of {peak} KiB misses the {LATE_ALLOCATION_KIB} KiB allocated just before exit"
            );
        }

        #[test]
        fn a_failing_process_reports_its_exit_status() {
            let dir = tempfile::tempdir().expect("a scratch directory");
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "echo broken >&2; exit 3"]);
            let error = exit_peak_rss_kib(dir.path(), command).expect_err("a failing run");
            assert!(error.contains("exit status: 3"), "{error}");
            assert!(error.contains("broken"), "{error}");
        }
    }
}
