use std::io;
use std::sync::Arc;

use ofx_trace::trace_log;

use crate::command_runner::{CORE, OutputStream, error_name};

const PENDING_FLUSH_BYTES: usize = 4096;

pub type OutputEcho = Arc<dyn Fn(&[u8]) -> io::Result<()> + Send + Sync>;

pub(crate) struct LineEcho {
    echo: OutputEcho,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    suppressed: bool,
}

impl LineEcho {
    pub(crate) fn new(echo: OutputEcho) -> Self {
        Self {
            echo,
            stdout: Vec::new(),
            stderr: Vec::new(),
            suppressed: false,
        }
    }

    pub(crate) fn push(&mut self, stream: OutputStream, bytes: &[u8]) {
        if self.suppressed {
            return;
        }
        let pending = match stream {
            OutputStream::Stdout => &mut self.stdout,
            OutputStream::Stderr => &mut self.stderr,
        };
        pending.extend_from_slice(bytes);
        let complete = pending
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |newline| newline + 1);
        let ready = if pending.len() - complete >= PENDING_FLUSH_BYTES {
            pending.len()
        } else {
            complete
        };
        let lines: Vec<u8> = pending.drain(..ready).collect();
        self.emit(&lines);
    }

    pub(crate) fn flush(&mut self) {
        let stdout = std::mem::take(&mut self.stdout);
        let stderr = std::mem::take(&mut self.stderr);
        self.emit(&stdout);
        self.emit(&stderr);
    }

    fn emit(&mut self, chunk: &[u8]) {
        if chunk.is_empty() || self.suppressed {
            return;
        }
        if let Err(error) = (self.echo)(chunk) {
            trace_log!(
                CORE,
                "command output presentation suppressed err={}",
                error_name(&error)
            );
            self.suppressed = true;
            self.stdout.clear();
            self.stderr.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn recorder(fail_after: usize) -> (LineEcho, Arc<Mutex<Vec<Vec<u8>>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let echo: OutputEcho = Arc::new(move |chunk: &[u8]| {
            let mut chunks = recorded.lock().unwrap();
            if chunks.len() == fail_after {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            chunks.push(chunk.to_vec());
            Ok(())
        });
        (LineEcho::new(echo), seen)
    }

    fn chunks(seen: &Mutex<Vec<Vec<u8>>>) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect()
    }

    #[test]
    fn complete_lines_are_echoed_as_they_arrive_and_partial_lines_at_the_end() {
        let (mut echo, seen) = recorder(usize::MAX);
        echo.push(OutputStream::Stdout, b"one\ntw");
        echo.push(OutputStream::Stderr, b"err");
        echo.push(OutputStream::Stdout, b"o\nthree");
        echo.push(OutputStream::Stderr, b"or\n");
        echo.push(OutputStream::Stdout, b"");
        echo.push(OutputStream::Stderr, b"tail");
        echo.flush();
        assert_eq!(
            chunks(&seen),
            ["one\n", "two\n", "error\n", "three", "tail"]
        );
    }

    #[test]
    fn long_partial_lines_are_echoed_once_they_fill_the_pending_buffer() {
        let (mut echo, seen) = recorder(usize::MAX);
        echo.push(OutputStream::Stdout, &[b'x'; PENDING_FLUSH_BYTES - 1]);
        assert!(chunks(&seen).is_empty());
        echo.push(OutputStream::Stdout, b"x");
        assert_eq!(chunks(&seen), ["x".repeat(PENDING_FLUSH_BYTES)]);
        echo.flush();
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    const TRACE_CHILD: &str = "OH_FX_OUTPUT_ECHO_TRACE_CHILD";
    const TRACE_TEST: &str =
        "output_echo::tests::a_failed_echo_writes_its_core_line_without_the_output";

    #[test]
    fn a_failed_echo_writes_its_core_line_without_the_output() {
        if let Some(root) = std::env::var_os(TRACE_CHILD) {
            ofx_trace::configure_from_env(std::path::Path::new(&root), None);
            let (mut echo, _) = recorder(0);
            echo.push(OutputStream::Stdout, b"secret output\n");
            echo.push(OutputStream::Stdout, b"more output\n");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("trace.log");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([TRACE_TEST, "--exact", "--test-threads=1"])
            .env_clear()
            .env(TRACE_CHILD, root.path())
            .env("OH_FX_TRACE_LOG", &log)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "{status:?}");
        let written = std::fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(
            written
                .matches(" [core] command output presentation suppressed err=BrokenPipe\n")
                .count(),
            1,
            "{written}"
        );
        assert!(!written.contains("output\n"), "{written}");
    }

    #[test]
    fn a_failed_echo_stops_echoing_that_command() {
        let (mut echo, seen) = recorder(1);
        echo.push(OutputStream::Stdout, b"first\n");
        echo.push(OutputStream::Stdout, b"second\nkept");
        echo.push(OutputStream::Stderr, b"third\n");
        echo.flush();
        assert_eq!(chunks(&seen), ["first\n"]);
    }
}
