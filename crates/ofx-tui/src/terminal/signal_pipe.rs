use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use signal_hook::SigId;
use signal_hook::consts::signal::{SIGCONT, SIGHUP, SIGTERM, SIGWINCH};

const FATAL_SIGNALS: [i32; 2] = [SIGTERM, SIGHUP];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Signals {
    pub(crate) resized: bool,
    pub(crate) continued: bool,
    pub(crate) fatal: Option<i32>,
}

pub(crate) struct SignalPipe {
    reader: UnixStream,
    fatal_reader: UnixStream,
    resized: Arc<AtomicBool>,
    continued: Arc<AtomicBool>,
    fatal: Arc<AtomicUsize>,
    default_disposition: Arc<AtomicBool>,
    ids: Vec<SigId>,
}

impl SignalPipe {
    pub(crate) fn install() -> io::Result<Self> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        let (fatal_reader, fatal_writer) = UnixStream::pair()?;
        let mut pipe = Self {
            reader,
            fatal_reader,
            resized: Arc::new(AtomicBool::new(false)),
            continued: Arc::new(AtomicBool::new(false)),
            fatal: Arc::new(AtomicUsize::new(0)),
            default_disposition: Arc::new(AtomicBool::new(false)),
            ids: Vec::new(),
        };
        pipe.ids.push(signal_hook::flag::register(
            SIGWINCH,
            Arc::clone(&pipe.resized),
        )?);
        pipe.ids.push(signal_hook::flag::register(
            SIGCONT,
            Arc::clone(&pipe.continued),
        )?);
        for signal in FATAL_SIGNALS {
            signal_hook::flag::register_conditional_default(
                signal,
                Arc::clone(&pipe.default_disposition),
            )?;
            pipe.ids.push(signal_hook::flag::register_usize(
                signal,
                Arc::clone(&pipe.fatal),
                usize::try_from(signal).unwrap_or_default(),
            )?);
            pipe.ids.push(signal_hook::low_level::pipe::register(
                signal,
                fatal_writer.try_clone()?,
            )?);
        }
        for signal in [SIGWINCH, SIGCONT, SIGTERM, SIGHUP] {
            pipe.ids.push(signal_hook::low_level::pipe::register(
                signal,
                writer.try_clone()?,
            )?);
        }
        Ok(pipe)
    }

    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.reader.as_fd()
    }

    pub(crate) fn fatal_wakeup(&self) -> io::Result<OwnedFd> {
        Ok(self.fatal_reader.try_clone()?.into())
    }

    pub(crate) fn take(&self) -> Signals {
        let mut sink = [0_u8; 64];
        while matches!((&self.reader).read(&mut sink), Ok(count) if count > 0) {}
        let fatal = self.fatal.swap(0, Ordering::SeqCst);
        Signals {
            resized: self.resized.swap(false, Ordering::SeqCst),
            continued: self.continued.swap(false, Ordering::SeqCst),
            fatal: (fatal != 0).then(|| i32::try_from(fatal).unwrap_or(SIGTERM)),
        }
    }

    pub(crate) fn uninstall(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
        self.default_disposition.store(true, Ordering::SeqCst);
    }
}

impl Drop for SignalPipe {
    fn drop(&mut self) {
        self.uninstall();
    }
}

pub(crate) fn raise_default(signal: i32) {
    let _ = signal_hook::low_level::emulate_default_handler(signal);
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHILD: &str = "OH_FX_SIGNAL_PIPE_CHILD";

    fn run_in_child(test: &str) -> std::process::ExitStatus {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--test-threads=1", "--nocapture"])
            .env(CHILD, "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
    }

    fn readable(fd: BorrowedFd<'_>, timeout_ms: u64) -> bool {
        let mut fds = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        let timeout =
            rustix::event::Timespec::try_from(std::time::Duration::from_millis(timeout_ms))
                .unwrap();
        rustix::event::poll(&mut fds, Some(&timeout)).unwrap() > 0
    }

    #[test]
    fn uninstalled_pipes_leave_the_default_termination_in_place() {
        if std::env::var_os(CHILD).is_some() {
            let mut pipe = SignalPipe::install().unwrap();
            pipe.uninstall();
            signal_hook::low_level::raise(SIGTERM).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(5));
            std::process::exit(0);
        }
        let status = run_in_child(
            "terminal::signal_pipe::tests::uninstalled_pipes_leave_the_default_termination_in_place",
        );
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(SIGTERM)
        );
    }

    #[test]
    fn raising_with_the_default_action_ends_the_process_with_that_signal() {
        if std::env::var_os(CHILD).is_some() {
            let mut pipe = SignalPipe::install().unwrap();
            pipe.uninstall();
            raise_default(SIGHUP);
            std::thread::sleep(std::time::Duration::from_secs(5));
            std::process::exit(0);
        }
        let status = run_in_child(
            "terminal::signal_pipe::tests::raising_with_the_default_action_ends_the_process_with_that_signal",
        );
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(SIGHUP)
        );
    }

    #[test]
    fn fatal_signals_wake_the_write_abort_descriptor() {
        if std::env::var_os(CHILD).is_some() {
            let pipe = SignalPipe::install().unwrap();
            let wakeup = pipe.fatal_wakeup().unwrap();
            signal_hook::low_level::raise(SIGWINCH).unwrap();
            let quiet = !readable(wakeup.as_fd(), 50);
            signal_hook::low_level::raise(SIGTERM).unwrap();
            let woken = readable(wakeup.as_fd(), 1000);
            let fatal = pipe.take().fatal == Some(SIGTERM);
            let still_woken = readable(wakeup.as_fd(), 0);
            std::process::exit(i32::from(!(quiet && woken && fatal && still_woken)));
        }
        let status = run_in_child(
            "terminal::signal_pipe::tests::fatal_signals_wake_the_write_abort_descriptor",
        );
        assert_eq!(status.code(), Some(0));
    }

    #[test]
    fn delivered_signals_set_flags_and_wake_the_pipe() {
        let pipe = SignalPipe::install().unwrap();
        signal_hook::low_level::raise(SIGWINCH).unwrap();
        signal_hook::low_level::raise(SIGCONT).unwrap();
        let mut ready = false;
        for _ in 0..100 {
            if readable(pipe.fd(), 10) {
                ready = true;
                break;
            }
        }
        assert!(ready);
        let signals = pipe.take();
        assert!(signals.resized);
        assert!(signals.continued);
        assert_eq!(signals.fatal, None);
        assert_eq!(pipe.take(), Signals::default());
    }
}
