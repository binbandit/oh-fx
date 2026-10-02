use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use signal_hook::SigId;
use signal_hook::consts::signal::{SIGCONT, SIGHUP, SIGTERM, SIGWINCH};

const FATAL_SIGNALS: [i32; 2] = [SIGTERM, SIGHUP];

static FATAL_FALLBACK: LazyLock<Mutex<FatalFallback>> = LazyLock::new(|| {
    Mutex::new(FatalFallback {
        armed: Arc::new(AtomicBool::new(true)),
        registered: false,
        live_pipes: 0,
    })
});

struct FatalFallback {
    armed: Arc<AtomicBool>,
    registered: bool,
    live_pipes: usize,
}

impl FatalFallback {
    fn lock() -> MutexGuard<'static, Self> {
        FATAL_FALLBACK
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn register() -> io::Result<()> {
        let mut fallback = Self::lock();
        if !fallback.registered {
            for signal in FATAL_SIGNALS {
                signal_hook::flag::register_conditional_default(
                    signal,
                    Arc::clone(&fallback.armed),
                )?;
            }
            fallback.registered = true;
        }
        Ok(())
    }

    fn suspend() {
        let mut fallback = Self::lock();
        fallback.live_pipes += 1;
        fallback.armed.store(false, Ordering::SeqCst);
    }

    fn resume() {
        let mut fallback = Self::lock();
        fallback.live_pipes -= 1;
        if fallback.live_pipes == 0 {
            fallback.armed.store(true, Ordering::SeqCst);
        }
    }
}

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
    suspends_fallback: bool,
    ids: Vec<SigId>,
}

impl SignalPipe {
    pub(crate) fn install() -> io::Result<Self> {
        FatalFallback::register()?;
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        let (fatal_reader, fatal_writer) = UnixStream::pair()?;
        let mut pipe = Self {
            reader,
            fatal_reader,
            resized: Arc::new(AtomicBool::new(false)),
            continued: Arc::new(AtomicBool::new(false)),
            fatal: Arc::new(AtomicUsize::new(0)),
            suspends_fallback: false,
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
        FatalFallback::suspend();
        pipe.suspends_fallback = true;
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
        if std::mem::take(&mut self.suspends_fallback) {
            FatalFallback::resume();
        }
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
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
    const INSTALL_DESCRIPTORS: usize = 10;

    fn run_in_child(test: &str) -> std::process::ExitStatus {
        run_in_child_raising(test, 0)
    }

    fn run_in_child_raising(test: &str, signal: i32) -> std::process::ExitStatus {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--test-threads=1", "--nocapture"])
            .env(CHILD, signal.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
    }

    fn signal_for_child() -> Option<i32> {
        std::env::var(CHILD).ok()?.parse().ok()
    }

    fn exit_with_interception(pipe: &SignalPipe, signal: i32) -> ! {
        signal_hook::low_level::raise(signal).unwrap();
        std::process::exit(i32::from(pipe.take().fatal != Some(signal)));
    }

    fn assert_children_intercept_each_fatal_signal(test: &str) {
        let statuses = FATAL_SIGNALS.map(|signal| (signal, run_in_child_raising(test, signal)));
        assert!(
            statuses.iter().all(|(_, status)| status.code() == Some(0)),
            "{statuses:?}"
        );
    }

    fn assert_children_die_by_each_fatal_signal(test: &str) {
        let statuses = FATAL_SIGNALS.map(|signal| (signal, run_in_child_raising(test, signal)));
        assert!(
            statuses.iter().all(|(signal, status)| {
                std::os::unix::process::ExitStatusExt::signal(status) == Some(*signal)
            }),
            "{statuses:?}"
        );
    }

    fn hold_every_descriptor_but(free: usize) -> Vec<std::fs::File> {
        let mut held = vec![std::fs::File::open("/dev/null").unwrap()];
        while let Ok(file) = held[0].try_clone() {
            held.push(file);
        }
        held.truncate(held.len() - free);
        held
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
    fn failed_installs_leave_the_default_termination_in_place() {
        if let Some(signal) = signal_for_child() {
            let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
            rustix::process::setrlimit(
                rustix::process::Resource::Nofile,
                rustix::process::Rlimit {
                    current: Some(limit.current.map_or(256, |current| current.min(256))),
                    maximum: limit.maximum,
                },
            )
            .unwrap();
            for free in 0..INSTALL_DESCRIPTORS {
                let _held = hold_every_descriptor_but(free);
                assert!(SignalPipe::install().is_err());
            }
            signal_hook::low_level::raise(signal).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(5));
            std::process::exit(0);
        }
        assert_children_die_by_each_fatal_signal(
            "terminal::signal_pipe::tests::failed_installs_leave_the_default_termination_in_place",
        );
    }

    #[test]
    fn reinstalled_pipes_intercept_fatal_signals_after_an_uninstall() {
        if let Some(signal) = signal_for_child() {
            let mut first = SignalPipe::install().unwrap();
            first.uninstall();
            let second = SignalPipe::install().unwrap();
            exit_with_interception(&second, signal);
        }
        assert_children_intercept_each_fatal_signal(
            "terminal::signal_pipe::tests::reinstalled_pipes_intercept_fatal_signals_after_an_uninstall",
        );
    }

    #[test]
    fn reinstalled_pipes_intercept_fatal_signals_after_a_drop() {
        if let Some(signal) = signal_for_child() {
            drop(SignalPipe::install().unwrap());
            let second = SignalPipe::install().unwrap();
            exit_with_interception(&second, signal);
        }
        assert_children_intercept_each_fatal_signal(
            "terminal::signal_pipe::tests::reinstalled_pipes_intercept_fatal_signals_after_a_drop",
        );
    }

    #[test]
    fn overlapping_pipes_intercept_fatal_signals_until_the_last_one_goes() {
        if let Some(signal) = signal_for_child() {
            let first = SignalPipe::install().unwrap();
            let second = SignalPipe::install().unwrap();
            drop(first);
            exit_with_interception(&second, signal);
        }
        assert_children_intercept_each_fatal_signal(
            "terminal::signal_pipe::tests::overlapping_pipes_intercept_fatal_signals_until_the_last_one_goes",
        );
    }

    #[test]
    fn reinstalled_pipes_restore_the_default_termination_once_torn_down() {
        if let Some(signal) = signal_for_child() {
            drop(SignalPipe::install().unwrap());
            let mut second = SignalPipe::install().unwrap();
            second.uninstall();
            signal_hook::low_level::raise(signal).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(5));
            std::process::exit(0);
        }
        assert_children_die_by_each_fatal_signal(
            "terminal::signal_pipe::tests::reinstalled_pipes_restore_the_default_termination_once_torn_down",
        );
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
