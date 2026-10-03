use std::any::Any;
use std::io;
use std::os::fd::AsFd;
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use ofx_contract::{Notice, NoticeTone};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;

const REPORT_WRITE_BUDGET: Duration = Duration::from_millis(100);
const PANIC_TOPIC: &str = "panic";

type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;
type Notify = Box<dyn Fn(Notice) + Send + Sync + 'static>;
type Slot = Arc<Mutex<Option<String>>>;

pub(crate) struct PanicCapture {
    shell: ThreadId,
    shell_active: Arc<AtomicBool>,
    shell_report: Slot,
    worker_report: Slot,
    previous: Arc<PanicHook>,
}

impl PanicCapture {
    pub(crate) fn install(
        worker_thread: &'static str,
        notify: impl Fn(Notice) + Send + Sync + 'static,
    ) -> Self {
        let shell = thread::current().id();
        let shell_active = Arc::new(AtomicBool::new(false));
        let shell_report: Slot = Arc::default();
        let worker_report: Slot = Arc::default();
        let previous: Arc<PanicHook> = Arc::new(panic::take_hook());
        let hook = Hook {
            shell,
            worker_thread,
            shell_active: Arc::clone(&shell_active),
            shell_report: Arc::clone(&shell_report),
            worker_report: Arc::clone(&worker_report),
            notify: Box::new(notify),
            fallback: Arc::clone(&previous),
        };
        panic::set_hook(Box::new(move |info| hook.capture(info)));
        Self {
            shell,
            shell_active,
            shell_report,
            worker_report,
            previous,
        }
    }

    pub(crate) fn contain_shell<T>(
        &self,
        shell: impl FnOnce() -> T,
    ) -> Result<T, Box<dyn Any + Send>> {
        debug_assert_eq!(thread::current().id(), self.shell);
        self.shell_active.store(true, Ordering::SeqCst);
        let outcome = panic::catch_unwind(AssertUnwindSafe(shell));
        self.shell_active.store(false, Ordering::SeqCst);
        outcome.inspect_err(|_| {
            if let Some(report) = take(&self.shell_report) {
                write_bounded(&io::stderr(), report.as_bytes(), REPORT_WRITE_BUDGET);
            }
        })
    }

    pub(crate) fn take_worker_report(&self) -> Option<String> {
        take(&self.worker_report)
    }
}

impl Drop for PanicCapture {
    fn drop(&mut self) {
        if thread::panicking() {
            return;
        }
        let previous = Arc::clone(&self.previous);
        drop(panic::take_hook());
        panic::set_hook(Box::new(move |info| previous(info)));
    }
}

struct Hook {
    shell: ThreadId,
    worker_thread: &'static str,
    shell_active: Arc<AtomicBool>,
    shell_report: Slot,
    worker_report: Slot,
    notify: Notify,
    fallback: Arc<PanicHook>,
}

impl Hook {
    fn capture(&self, info: &PanicHookInfo<'_>) {
        let current = thread::current();
        let shell_active = self.shell_active.load(Ordering::SeqCst);
        let report = thread_report(current.name(), info);
        if current.id() == self.shell {
            if shell_active {
                store(&self.shell_report, report);
            } else {
                (self.fallback)(info);
            }
            return;
        }
        let worker = current.name() == Some(self.worker_thread);
        if worker {
            store(&self.worker_report, panic_report(info));
        }
        if shell_active {
            (self.notify)(Notice::new(
                NoticeTone::Error,
                PANIC_TOPIC,
                report.trim_end(),
            ));
        } else if !worker {
            (self.fallback)(info);
        }
    }
}

fn store(slot: &Slot, report: String) {
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(report);
}

fn take(slot: &Slot) -> Option<String> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

pub(crate) fn panic_report(info: &PanicHookInfo<'_>) -> String {
    let message = payload_text(info.payload());
    match info.location() {
        Some(location) => format!("panicked at {location}: {message}"),
        None => format!("panicked: {message}"),
    }
}

fn thread_report(name: Option<&str>, info: &PanicHookInfo<'_>) -> String {
    format!(
        "thread '{}' {}\n",
        name.unwrap_or("<unnamed>"),
        panic_report(info)
    )
}

fn payload_text(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("Box<dyn Any>")
}

fn write_bounded(output: &impl AsFd, mut bytes: &[u8], budget: Duration) {
    let fd = output.as_fd();
    let Ok(flags) = fcntl_getfl(fd) else {
        return;
    };
    if fcntl_setfl(fd, flags | OFlags::NONBLOCK).is_err() {
        return;
    }
    let deadline = Instant::now() + budget;
    while !bytes.is_empty() {
        match rustix::io::write(fd, bytes) {
            Ok(written) => bytes = &bytes[written..],
            Err(Errno::INTR) => {}
            Err(Errno::AGAIN) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let Ok(timeout) = Timespec::try_from(remaining) else {
                    break;
                };
                let mut fds = [PollFd::new(&fd, PollFlags::OUT)];
                if remaining.is_zero() || !matches!(poll(&mut fds, Some(&timeout)), Ok(1..)) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = fcntl_setfl(fd, flags);
}

#[cfg(test)]
pub(crate) static HOOK_TESTS: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use ofx_contract::{PermissionMode, UiCommand, UiEvent};
    use ofx_testkit::PtySession;
    use ofx_tui::{Opening, PromptHistory, ShellOptions, run_shell, ui_channel};
    use rustix::fs::Mode;

    use super::*;
    use crate::native::NativeClipboard;

    const CHILD: &str = "OH_FX_PANIC_CAPTURE_CHILD";
    const CHILD_TEST: &str =
        "app_panic_runtime::tests::a_shell_panic_on_a_stalled_terminal_restores_it_and_exits";
    const FIRST_FRAME: &[u8] = b"Run /help for commands";
    const WAIT: Duration = Duration::from_secs(15);

    #[test]
    fn bounded_writes_give_up_on_an_output_that_stops_reading() {
        let (reader, writer) = io::pipe().unwrap();
        let started = Instant::now();
        write_bounded(&writer, &vec![b'x'; 1 << 20], Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!fcntl_getfl(&writer).unwrap().contains(OFlags::NONBLOCK));
        drop(reader);
    }

    #[test]
    fn background_panics_while_the_shell_runs_become_notices() {
        let _serial = HOOK_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
        let notices = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&notices);
        let panics = PanicCapture::install("unused", move |notice| {
            collected.lock().unwrap().push(notice);
        });
        let crashed = panics
            .contain_shell(|| {
                thread::Builder::new()
                    .name("tool".to_owned())
                    .spawn(|| panic!("tool exploded"))
                    .unwrap()
                    .join()
                    .is_err()
            })
            .unwrap();
        drop(panics);
        assert!(crashed);
        let notices = notices.lock().unwrap();
        let ours: Vec<_> = notices
            .iter()
            .filter(|notice| notice.body.starts_with("thread 'tool' "))
            .collect();
        assert_eq!(ours.len(), 1, "{notices:?}");
        assert_eq!(ours[0].topic, "panic");
        assert_eq!(ours[0].tone, NoticeTone::Error);
        assert!(
            ours[0].body.starts_with("thread 'tool' panicked at "),
            "{}",
            ours[0].body
        );
        assert!(
            ours[0].body.ends_with(": tool exploded"),
            "{}",
            ours[0].body
        );
    }

    #[test]
    fn a_shell_panic_on_a_stalled_terminal_restores_it_and_exits() {
        if let Some(handshake) = std::env::var_os(CHILD) {
            run_a_shell_that_panics(Path::new(&handshake));
        }
        let directory = tempfile::tempdir().unwrap();
        let handshake = directory.path().join("stalled");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([CHILD_TEST, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, &handshake)
            .env("TERM", "xterm-256color");
        let mut session = PtySession::spawn(command, 24, 80).unwrap();
        session.stall_output_after(FIRST_FRAME).unwrap();
        wait_until(|| {
            session
                .output()
                .windows(FIRST_FRAME.len())
                .any(|window| window == FIRST_FRAME)
        });
        assert!(!session.cooked().unwrap());
        fs::write(&handshake, "").unwrap();
        wait_until(|| full(&handshake).exists());
        session.send(b"go\r");
        let status = session
            .wait_exit(WAIT)
            .expect("a panic on the shell thread ends the process");
        assert_eq!(status.code(), Some(101));
        assert!(session.cooked().unwrap());
    }

    fn wait_until(ready: impl Fn() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !ready() {
            assert!(Instant::now() < deadline, "timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn full(handshake: &Path) -> PathBuf {
        handshake.with_extension("full")
    }

    fn run_a_shell_that_panics(handshake: &Path) -> ! {
        let (notices, events) = ui_channel().unwrap();
        let panics = PanicCapture::install("unused", move |notice| {
            notices.send(UiEvent::Notice { notice });
        });
        let handshake = handshake.to_owned();
        thread::spawn(move || fill_the_terminal(&handshake));
        let options = ShellOptions {
            version: "0.1.0".to_owned(),
            model: "model-a".to_owned(),
            permission_mode: PermissionMode::Auto,
            full_access_warning: false,
            workspace_label: "workspace".to_owned(),
            workspace_root: PathBuf::from("/workspace"),
            startup_scrollback: true,
            commands: Vec::new(),
            command_categories: Vec::new(),
            prompt_history: PromptHistory::disabled(),
            file_mentions: None,
            skill_catalog: None,
            opening: Opening::Welcome,
        };
        let outcome = panics.contain_shell(|| {
            run_shell(options, events, NativeClipboard, |command| {
                assert!(
                    !matches!(command, UiCommand::Submit { .. }),
                    "shell exploded"
                );
            })
        });
        std::process::exit(if outcome.is_err() { 101 } else { 0 });
    }

    fn fill_the_terminal(handshake: &Path) {
        wait_until(|| handshake.exists());
        let name = rustix::termios::ttyname(io::stdin(), Vec::new()).unwrap();
        let terminal = rustix::fs::open(
            name.as_c_str(),
            OFlags::WRONLY | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let junk = [b'.'; 4096];
        while let Ok(_) | Err(Errno::INTR) = rustix::io::write(&terminal, &junk) {}
        fs::write(full(handshake), "").unwrap();
    }
}
