use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use ofx_contract::{PermissionMode, UiCommand, UiEvent};
use ofx_testkit::PtyPair;
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::termios::Winsize;

use super::{FOOTER_ROWS, Setup, Shell, ShellOptions, SlashCommandSpec, UiEventSender, ui_channel};
use crate::input::{COMPOSER_INPUT_LIMIT_BYTES, TerminalInput};
use crate::terminal::signal_pipe::SignalPipe;
use crate::terminal::test_pty;
use crate::theme::Theme;

const ROWS: u16 = 24;
const COLS: u16 = 80;

pub(super) struct TestShell {
    pty: PtyPair,
    screen: vt100::Parser,
    events: UiEventSender,
    commands: Rc<RefCell<Vec<UiCommand>>>,
    pub(super) shell: Shell<'static>,
}

impl TestShell {
    pub(super) fn start() -> Self {
        let pty = PtyPair::open(ROWS, COLS).unwrap();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        let layout = terminal.query_layout(FOOTER_ROWS).unwrap();
        let (events, receiver) = ui_channel().unwrap();
        let commands = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&commands);
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
            options(),
            receiver,
            Box::new(move |command| sink.borrow_mut().push(command)),
        );
        Self {
            pty,
            screen: vt100::Parser::new(ROWS, COLS, 0),
            events,
            commands,
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
        let now_ms = self.shell.now_ms();
        self.shell.apply_pending_resize(now_ms);
    }

    pub(super) fn written(&mut self) -> String {
        self.shell.commit_frame().unwrap();
        let output = drain(&self.pty);
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
        assert!(self.shell.step().unwrap().is_none());
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

fn drain(pty: &PtyPair) -> Vec<u8> {
    let mut written = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let mut fds = [PollFd::new(&pty.master, PollFlags::IN)];
        let timeout = Timespec::try_from(Duration::from_millis(20)).unwrap();
        if rustix::event::poll(&mut fds, Some(&timeout)).unwrap() == 0 {
            return written;
        }
        let count = rustix::io::read(&pty.master, &mut buffer).unwrap();
        written.extend_from_slice(&buffer[..count]);
    }
}

fn options() -> ShellOptions {
    let spec = |command: &str, aliases: &[&str]| SlashCommandSpec {
        command: command.to_owned(),
        aliases: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
        description: String::new(),
    };
    ShellOptions {
        version: "0.1.0".to_owned(),
        model: "model-a".to_owned(),
        permission_mode: PermissionMode::Auto,
        workspace_label: "workspace".to_owned(),
        workspace_root: PathBuf::from("/workspace"),
        commands: vec![
            spec("/help", &[]),
            spec("/clear", &[]),
            spec("/model", &[]),
            spec("/quit", &["/exit"]),
        ],
    }
}
