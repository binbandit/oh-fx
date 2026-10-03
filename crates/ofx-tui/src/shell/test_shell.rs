use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ofx_contract::{PermissionMode, UiCommand, UiEvent};
use ofx_testkit::PtyPair;
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::termios::Winsize;

use super::{
    FOOTER_ROWS, Opening, PromptHistory, Setup, Shell, ShellOptions, SlashCommandSpec,
    UiEventSender, ui_channel,
};
use crate::host::Clipboard;
use crate::input::{COMPOSER_INPUT_LIMIT_BYTES, TerminalInput};
use crate::terminal::signal_pipe::SignalPipe;
use crate::terminal::test_pty;
use crate::theme::Theme;

const ROWS: u16 = 24;
const COLS: u16 = 80;

pub(super) struct TestShell {
    pty: PtyPair,
    screen: vt100::Parser,
    output: Vec<u8>,
    events: UiEventSender,
    commands: Rc<RefCell<Vec<UiCommand>>>,
    pub(super) clipboard: Arc<TestClipboard>,
    pub(super) shell: Shell<'static>,
}

#[derive(Default)]
pub(super) struct TestClipboard {
    copied: Mutex<Vec<String>>,
    pub(super) fails: AtomicBool,
    held: Mutex<bool>,
    released: Condvar,
}

impl TestClipboard {
    pub(super) fn hold(&self) {
        *self.held.lock().unwrap() = true;
    }

    pub(super) fn release(&self) {
        *self.held.lock().unwrap() = false;
        self.released.notify_all();
    }
}

impl Clipboard for TestClipboard {
    fn copy(&self, text: &str) -> bool {
        drop(
            self.released
                .wait_while(self.held.lock().unwrap(), |held| *held)
                .unwrap(),
        );
        self.copied.lock().unwrap().push(text.to_owned());
        !self.fails.load(Ordering::Acquire)
    }
}

impl TestShell {
    pub(super) fn start() -> Self {
        Self::start_with(|_| {})
    }

    pub(super) fn start_with(configure: impl FnOnce(&mut ShellOptions)) -> Self {
        let mut options = options();
        configure(&mut options);
        let pty = PtyPair::open(ROWS, COLS).unwrap();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        let layout = terminal.query_layout(FOOTER_ROWS).unwrap();
        let (events, receiver) = ui_channel().unwrap();
        let commands = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&commands);
        let clipboard = Arc::new(TestClipboard::default());
        let setup = Setup {
            terminal,
            signals: SignalPipe::install().unwrap(),
            input: TerminalInput::new(),
            layout,
            theme: Theme::builtin(false, true, true),
            theme_pinned: true,
            launch_row: 1,
        };
        let shell = Shell::assemble(
            setup,
            options,
            receiver,
            Arc::clone(&clipboard) as Arc<dyn Clipboard>,
            Box::new(move |command| sink.borrow_mut().push(command)),
        );
        Self {
            pty,
            screen: vt100::Parser::new(ROWS, COLS, 0),
            output: Vec::new(),
            events,
            commands,
            clipboard,
            shell,
        }
    }

    pub(super) fn deliver(&mut self, event: UiEvent) {
        self.queue(event);
        self.shell.drain_ui_events();
    }

    pub(super) fn queue(&self, event: UiEvent) {
        self.events.send(event);
    }

    pub(super) fn type_bytes(&self, bytes: &[u8]) {
        rustix::io::write(&self.pty.master, bytes).unwrap();
    }

    pub(super) fn resize(&mut self, rows: u16, cols: u16) {
        rustix::termios::tcsetwinsize(&self.pty.master, winsize(rows, cols)).unwrap();
        self.shell.resize_due_ms = Some(0);
        self.draining(|shell| {
            let now_ms = shell.now_ms();
            shell.apply_pending_resize(now_ms);
        });
    }

    pub(super) fn draining<T>(&mut self, action: impl FnOnce(&mut Shell<'static>) -> T) -> T {
        self.draining_after(Duration::ZERO, action).0
    }

    pub(super) fn draining_after<T>(
        &mut self,
        delay: Duration,
        action: impl FnOnce(&mut Shell<'static>) -> T,
    ) -> (T, usize) {
        let done = AtomicBool::new(false);
        let master = &self.pty.master;
        let shell = &mut self.shell;
        let (result, output) = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                std::thread::sleep(delay);
                drain(master, &done)
            });
            let result = {
                let _finished = Finished(&done);
                action(shell)
            };
            (result, reader.join().unwrap())
        });
        self.output.extend_from_slice(&output);
        (result, output.len())
    }

    pub(super) fn written(&mut self) -> String {
        self.draining(|shell| shell.commit_frame().unwrap());
        let output = std::mem::take(&mut self.output);
        self.screen.process(&output);
        String::from_utf8_lossy(&output).into_owned()
    }

    pub(super) fn advance(&mut self, millis: u64) {
        self.shell.clock = self
            .shell
            .clock
            .checked_sub(Duration::from_millis(millis))
            .unwrap();
    }

    pub(super) fn step(&mut self) {
        assert!(self.draining(|shell| shell.step().unwrap().is_none()));
    }

    pub(super) fn submit(&mut self, text: &str) {
        self.shell.invalidate();
        self.shell
            .composer
            .insert_text(text, COMPOSER_INPUT_LIMIT_BYTES);
        self.shell.submit();
    }

    pub(super) fn sent(&self) -> Vec<UiCommand> {
        self.commands.borrow().clone()
    }

    pub(super) fn copied(&self) -> Vec<String> {
        self.clipboard.copied.lock().unwrap().clone()
    }

    pub(super) fn screen(&mut self) -> String {
        self.written();
        self.screen.screen().contents()
    }
}

fn winsize(rows: u16, cols: u16) -> Winsize {
    Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

struct Finished<'a>(&'a AtomicBool);

impl Drop for Finished<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn drain(master: &OwnedFd, done: &AtomicBool) -> Vec<u8> {
    let mut written = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let finished = done.load(Ordering::Acquire);
        let mut fds = [PollFd::new(master, PollFlags::IN)];
        let timeout = Timespec::try_from(Duration::from_millis(20)).unwrap();
        if rustix::event::poll(&mut fds, Some(&timeout)).unwrap() == 0 {
            if finished {
                return written;
            }
            continue;
        }
        let count = rustix::io::read(master, &mut buffer).unwrap();
        written.extend_from_slice(&buffer[..count]);
    }
}

fn options() -> ShellOptions {
    let spec = |command: &str, aliases: &[&str], category: usize| SlashCommandSpec {
        command: command.to_owned(),
        aliases: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
        description: String::new(),
        category,
        compacts: false,
    };
    ShellOptions {
        version: "0.1.0".to_owned(),
        model: "model-a".to_owned(),
        permission_mode: PermissionMode::Auto,
        full_access_warning: false,
        workspace_label: "workspace".to_owned(),
        workspace_root: PathBuf::from("/workspace"),
        commands: vec![
            spec("/help", &[], 0),
            spec("/clear", &[], 0),
            spec("/model", &[], 1),
            spec("/quit", &["/exit"], 0),
        ],
        command_categories: vec!["General".to_owned(), "Model".to_owned()],
        prompt_history: PromptHistory::enabled(Vec::new(), |_| Ok(())),
        file_mentions: None,
        steering: None,
        opening: Opening::Welcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicking_action_still_stops_the_reader() {
        if test_pty::in_child() {
            let mut test = TestShell::start();
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                test.draining(|_| panic!("the action failed"));
            }));
            std::process::exit(i32::from(caught.is_ok()));
        }
        let pty = test_pty::open();
        let child = test_pty::spawn_on(
            &pty,
            "shell::test_shell::tests::a_panicking_action_still_stops_the_reader",
        );
        let status = test_pty::exit_within(child, test_pty::WAIT);
        assert!(status.is_some_and(|status| status.success()), "{status:?}");
    }
}
