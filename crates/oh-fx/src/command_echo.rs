use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ofx_exec::OutputEcho;

#[derive(Default)]
pub(crate) struct CommandEcho {
    line_open: AtomicBool,
}

impl CommandEcho {
    pub(crate) fn output(self: &Arc<Self>) -> OutputEcho {
        let echo = Arc::clone(self);
        Arc::new(move |chunk: &[u8]| echo.write(&mut io::stderr().lock(), chunk))
    }

    pub(crate) fn finish_line(&self) -> io::Result<()> {
        self.close_line(&mut io::stderr().lock())
    }

    fn write(&self, stderr: &mut impl Write, chunk: &[u8]) -> io::Result<()> {
        stderr.write_all(chunk)?;
        stderr.flush()?;
        if let Some(last) = chunk.last() {
            self.line_open.store(*last != b'\n', Ordering::SeqCst);
        }
        Ok(())
    }

    fn close_line(&self, stderr: &mut impl Write) -> io::Result<()> {
        if !self.line_open.load(Ordering::SeqCst) {
            return Ok(());
        }
        stderr.write_all(b"\n")?;
        stderr.flush()?;
        self.line_open.store(false, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_that_ends_mid_line_gets_one_closing_newline() {
        let echo = CommandEcho::default();
        let mut stderr = Vec::new();
        echo.write(&mut stderr, b"one\ntwo").unwrap();
        echo.write(&mut stderr, b"").unwrap();
        echo.close_line(&mut stderr).unwrap();
        echo.close_line(&mut stderr).unwrap();
        echo.write(&mut stderr, b"three\n").unwrap();
        echo.close_line(&mut stderr).unwrap();
        assert_eq!(stderr, b"one\ntwo\nthree\n");
    }
}
