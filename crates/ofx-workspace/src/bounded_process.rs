use std::io::{self, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;

const STDERR_LIMIT: u64 = 1024;

pub(crate) struct ProcessOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
}

pub(crate) fn run_bounded(command: &mut Command, stdout_limit: usize) -> Option<ProcessOutput> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stderr = child.stderr.take()?;
    let stderr_reader = thread::spawn(move || io::copy(&mut stderr, &mut io::sink()));
    let stdout = child
        .stdout
        .take()
        .and_then(|stdout| read_limited(stdout, stdout_limit));
    if stdout.is_none() {
        let _ = child.kill();
    }
    let stderr_within_limit = stderr_reader
        .join()
        .is_ok_and(|copied| copied.is_ok_and(|bytes| bytes <= STDERR_LIMIT));
    let status = child.wait().ok()?;
    let stdout = stdout?;
    stderr_within_limit.then_some(ProcessOutput { status, stdout })
}

fn read_limited(reader: impl Read, limit: usize) -> Option<Vec<u8>> {
    let mut retained = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut retained)
        .ok()?;
    (retained.len() <= limit).then_some(retained)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_stdout_within_limits() {
        let output = run_bounded(Command::new("/bin/sh").args(["-c", "printf abc"]), 16)
            .expect("process output");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"abc");
    }

    #[test]
    fn rejects_output_beyond_limits() {
        assert!(run_bounded(Command::new("/bin/sh").args(["-c", "printf abcdef"]), 3).is_none());
        let noisy = format!("head -c {} /dev/zero >&2", STDERR_LIMIT + 1);
        assert!(run_bounded(Command::new("/bin/sh").args(["-c", &noisy]), 16).is_none());
        let quiet = format!("head -c {STDERR_LIMIT} /dev/zero >&2");
        assert!(run_bounded(Command::new("/bin/sh").args(["-c", &quiet]), 16).is_some());
    }
}
