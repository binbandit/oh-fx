use rustix::process::Signal;

use super::cursor_probe::CursorPosition;
use super::{
    Layout, Terminal, TerminalError, move_cursor_sequence, unstacked_interactive_mode_sequence,
};

const ABNORMAL_EXIT_RESTORE: &str = "\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1004l\x1b[?1006l\x1b[?1l\x1b>\x1b[?1049l\x1b[?7h\x1b[4l\x1b[?6l\x1b[0m\x1b[?25h\x1b[?2031l\x1b[?2004l\x1b[<u\x1b[>4;0m\n";
const TMUX_ABNORMAL_EXIT_RESTORE: &str = "\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1004l\x1b[?1006l\x1b[?1l\x1b>\x1b[?1049l\x1b[?7h\x1b[4l\x1b[?6l\x1b[0m\x1b[?25h\x1b[?2031l\x1b[?2004l\x1b[>4;0m\n";
const NORMAL_EXIT_RESTORE: &str = "\x1b[?2031l\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1004l\x1b[?1006l\x1b[?1l\x1b>\x1b[4l\x1b[?6l\x1b[?2004l\x1b[<u\x1b[>4;0m";
const TMUX_NORMAL_EXIT_RESTORE: &str = "\x1b[?2031l\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1004l\x1b[?1006l\x1b[?1l\x1b>\x1b[4l\x1b[?6l\x1b[?2004l\x1b[>4;0m";

pub(crate) fn normal_exit_restore_sequence(tmux: bool) -> &'static str {
    if tmux {
        TMUX_NORMAL_EXIT_RESTORE
    } else {
        NORMAL_EXIT_RESTORE
    }
}

pub(crate) fn abnormal_exit_restore_sequence(tmux: bool) -> &'static str {
    if tmux {
        TMUX_ABNORMAL_EXIT_RESTORE
    } else {
        ABNORMAL_EXIT_RESTORE
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExitCleanup {
    pub(crate) footer_top: Option<u16>,
    pub(crate) cursor_row: u16,
    pub(crate) rows: u16,
    pub(crate) sync_updates: bool,
}

impl ExitCleanup {
    fn row(&self) -> u16 {
        let row = self.footer_top.unwrap_or(self.cursor_row);
        if row == 0 {
            1
        } else if self.rows == 0 {
            row
        } else {
            row.min(self.rows)
        }
    }

    fn sequence(&self) -> String {
        let sync_end = if self.sync_updates { "\x1b[?2026l" } else { "" };
        let cursor = move_cursor_sequence(self.row(), 1);
        format!("{sync_end}\x1b[?7h\x1b[0m{cursor}\x1b[J\x1b[?25h\n")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StartupViewport {
    pub(crate) launch_row: u16,
    pub(crate) scrollback_rows: u16,
    pub(crate) reserved_rows: u16,
}

impl StartupViewport {
    pub(crate) fn plan(
        layout: Layout,
        launch_row: u16,
        startup_min_body_rows: u16,
        startup_scrollback: bool,
    ) -> Self {
        let mut plan = Self {
            launch_row,
            scrollback_rows: 0,
            reserved_rows: startup_min_body_rows,
        };
        if startup_scrollback && launch_row > 1 {
            plan.scrollback_rows = launch_row - 1;
            plan.launch_row = 1;
        } else if layout.content_bottom > 0 {
            let reserve_rows = startup_min_body_rows.min(layout.content_bottom);
            let max_start_row = if reserve_rows > 0 {
                layout.content_bottom - reserve_rows + 1
            } else {
                layout.content_bottom
            };
            if launch_row > max_start_row {
                plan.scrollback_rows = launch_row - max_start_row;
                plan.launch_row = max_start_row;
            }
        }
        plan
    }

    pub(crate) fn launch_row_from_cursor(cursor: Option<CursorPosition>, layout: Layout) -> u16 {
        let cursor = cursor.unwrap_or(CursorPosition {
            row: layout.content_bottom,
            col: 1,
        });
        if cursor.col > 1 && cursor.row < layout.rows {
            cursor.row + 1
        } else {
            cursor.row
        }
    }
}

fn launch_scrollback_sequence(layout: Layout, rows: u16) -> String {
    if rows == 0 || layout.rows == 0 {
        return String::new();
    }
    let mut sequence = move_cursor_sequence(layout.rows, 1);
    sequence.extend(std::iter::repeat_n('\n', usize::from(rows)));
    sequence
}

impl Terminal {
    pub(crate) fn push_launch_rows_into_scrollback(
        &self,
        layout: Layout,
        rows: u16,
    ) -> Result<(), TerminalError> {
        self.write_all(launch_scrollback_sequence(layout, rows).as_bytes())
    }

    pub(crate) fn leave_interactive_mode(&self) -> Result<(), TerminalError> {
        self.write_all(normal_exit_restore_sequence(self.capabilities().tmux).as_bytes())
    }

    pub(crate) fn restore_cooked_mode(&mut self, cleanup: &ExitCleanup) {
        self.disable_raw_mode();
        let _ = self.write_all(cleanup.sequence().as_bytes());
    }

    pub(crate) fn shutdown(&mut self, cleanup: &ExitCleanup) {
        let _ = self.leave_interactive_mode();
        self.restore_cooked_mode(cleanup);
    }

    pub(crate) fn restore_after_signal(&mut self) {
        self.release_raw_mode();
        self.write_abnormal_restore();
    }

    pub(crate) fn suspend_to_job_control(
        &mut self,
        cleanup: &ExitCleanup,
        footer_rows: u16,
    ) -> Result<Option<Layout>, TerminalError> {
        self.shutdown(cleanup);
        rustix::process::kill_process(rustix::process::getpid(), Signal::TSTP)?;
        let layout = self.query_layout(footer_rows).ok();
        self.capture_original_termios()?;
        self.enable_raw_mode()?;
        self.enter_interactive_mode()?;
        Ok(layout)
    }

    pub(crate) fn reclaim_after_external_stop(&mut self) -> Result<bool, TerminalError> {
        if !self.raw_mode_lost()? {
            return Ok(false);
        }
        self.capture_original_termios()?;
        self.enable_raw_mode()?;
        self.write_all(unstacked_interactive_mode_sequence().as_bytes())?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use ofx_testkit::PtyPair;
    use rustix::termios::OptionalActions;

    use super::super::shell_runtime::test_pty;
    use super::*;

    fn layout_24x80() -> Layout {
        Layout::from_size(24, 80, 4).unwrap()
    }

    #[test]
    fn abnormal_exit_restoration_leaves_the_alternate_screen() {
        let abnormal = abnormal_exit_restore_sequence(false);
        let normal = normal_exit_restore_sequence(false);
        assert!(abnormal.starts_with("\x1b[?2026l"));
        assert!(abnormal.find("\x1b[?2026l").unwrap() < abnormal.find("\x1b[?1049l").unwrap());
        assert!(normal.starts_with("\x1b[?2031l\x1b[?2026l"));
        assert!(abnormal.contains("\x1b[?1000l"));
        assert!(abnormal.contains("\x1b[?1006l"));
        assert!(abnormal.contains("\x1b[?1049l"));
        assert!(abnormal.contains("\x1b[?2031l"));
        assert!(normal.contains("\x1b[?2031l"));
    }

    #[test]
    fn terminal_keyboard_stack_restore_stays_paired_with_enable_policy() {
        assert!(normal_exit_restore_sequence(false).contains("\x1b[<u"));
        assert!(abnormal_exit_restore_sequence(false).contains("\x1b[<u"));

        let tmux_restore = normal_exit_restore_sequence(true);
        assert!(!tmux_restore.contains("\x1b[<u"));
        assert!(!abnormal_exit_restore_sequence(true).contains("\x1b[<u"));
        assert!(tmux_restore.contains("\x1b[?2004l"));
        assert!(tmux_restore.contains("\x1b[>4;0m"));
    }

    #[test]
    fn tmux_restore_sequences_differ_only_by_the_kitty_pop() {
        assert_eq!(
            normal_exit_restore_sequence(false).replace("\x1b[<u", ""),
            normal_exit_restore_sequence(true)
        );
        assert_eq!(
            abnormal_exit_restore_sequence(false).replace("\x1b[<u", ""),
            abnormal_exit_restore_sequence(true)
        );
    }

    #[test]
    fn launch_scrollback_push_creates_top_of_viewport_space() {
        assert_eq!(
            launch_scrollback_sequence(layout_24x80(), 3),
            "\x1b[24;1H\n\n\n"
        );
    }

    #[test]
    fn prepare_startup_viewport_uses_scrollback_setting_for_launch_push_only() {
        let layout = layout_24x80();
        let enabled = StartupViewport::plan(layout, 6, 11, true);
        assert_eq!(enabled.launch_row, 1);
        assert_eq!(enabled.reserved_rows, 11);
        assert_eq!(
            launch_scrollback_sequence(layout, enabled.scrollback_rows),
            "\x1b[24;1H\n\n\n\n\n"
        );

        let disabled = StartupViewport::plan(layout, 6, 11, false);
        assert_eq!(disabled.launch_row, 6);
        assert_eq!(disabled.reserved_rows, 11);
        assert_eq!(
            launch_scrollback_sequence(layout, disabled.scrollback_rows),
            ""
        );
    }

    #[test]
    fn startup_viewport_pushes_rows_to_reserve_body_space_without_scrollback_setting() {
        let plan = StartupViewport::plan(layout_24x80(), 18, 6, false);
        assert_eq!(plan.launch_row, 15);
        assert_eq!(plan.scrollback_rows, 3);
    }

    #[test]
    fn startup_launch_row_starts_on_a_fresh_line() {
        let layout = layout_24x80();
        assert_eq!(
            StartupViewport::launch_row_from_cursor(
                Some(CursorPosition { row: 5, col: 9 }),
                layout
            ),
            6
        );
        assert_eq!(
            StartupViewport::launch_row_from_cursor(
                Some(CursorPosition { row: 5, col: 1 }),
                layout
            ),
            5
        );
        assert_eq!(
            StartupViewport::launch_row_from_cursor(
                Some(CursorPosition { row: 24, col: 9 }),
                layout
            ),
            24
        );
        assert_eq!(StartupViewport::launch_row_from_cursor(None, layout), 20);
    }

    #[test]
    fn shutdown_cleanup_erases_from_footer_frame_top_after_frame_commit() {
        let cleanup = ExitCleanup {
            footer_top: Some(21),
            cursor_row: 6,
            rows: 24,
            sync_updates: false,
        };
        let bytes = cleanup.sequence();
        assert!(!bytes.contains("\x1b[6;1H\x1b[J"));
        assert!(bytes.contains("\x1b[21;1H\x1b[J"));
        assert_eq!(bytes, "\x1b[?7h\x1b[0m\x1b[21;1H\x1b[J\x1b[?25h\n");
    }

    #[test]
    fn shutdown_cleanup_ends_synchronized_output_and_clamps_the_row() {
        let cleanup = ExitCleanup {
            footer_top: None,
            cursor_row: 40,
            rows: 24,
            sync_updates: true,
        };
        assert_eq!(
            cleanup.sequence(),
            "\x1b[?2026l\x1b[?7h\x1b[0m\x1b[24;1H\x1b[J\x1b[?25h\n"
        );
        let unset = ExitCleanup {
            footer_top: None,
            cursor_row: 0,
            rows: 24,
            sync_updates: false,
        };
        assert!(unset.sequence().contains("\x1b[1;1H"));
    }

    fn drain(pty: &PtyPair) -> Vec<u8> {
        let mut written = Vec::new();
        let mut buffer = [0_u8; 512];
        loop {
            let mut fds = [rustix::event::PollFd::new(
                &pty.master,
                rustix::event::PollFlags::IN,
            )];
            let timeout =
                rustix::event::Timespec::try_from(std::time::Duration::from_millis(50)).unwrap();
            if rustix::event::poll(&mut fds, Some(&timeout)).unwrap() == 0 {
                return written;
            }
            let count = rustix::io::read(&pty.master, &mut buffer).unwrap();
            written.extend_from_slice(&buffer[..count]);
        }
    }

    #[test]
    fn a_continue_without_a_stop_leaves_the_terminal_alone() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        terminal.enter_interactive_mode().unwrap();
        drain(&pty);
        assert!(!terminal.reclaim_after_external_stop().unwrap());
        assert!(drain(&pty).is_empty());
        terminal.disable_raw_mode();
    }

    #[test]
    fn an_external_stop_reclaims_raw_mode_without_another_keyboard_push() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        let cooked = rustix::termios::tcgetattr(&pty.slave).unwrap();
        terminal.enable_raw_mode().unwrap();
        terminal.enter_interactive_mode().unwrap();
        drain(&pty);
        rustix::termios::tcsetattr(&pty.slave, OptionalActions::Now, &cooked).unwrap();
        assert!(terminal.reclaim_after_external_stop().unwrap());
        let written = String::from_utf8(drain(&pty)).unwrap();
        assert_eq!(written, unstacked_interactive_mode_sequence());
        assert!(!terminal.raw_mode_lost().unwrap());
        assert!(!terminal.reclaim_after_external_stop().unwrap());
        terminal.disable_raw_mode();
        let restored = rustix::termios::tcgetattr(&pty.slave).unwrap();
        assert!(
            restored
                .local_modes
                .contains(rustix::termios::LocalModes::ICANON)
        );
    }

    fn drain_while(pty: &PtyPair, write: impl FnOnce()) -> Vec<u8> {
        let finished = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let mut written = Vec::new();
                while !finished.load(Ordering::SeqCst) {
                    written.extend(drain(pty));
                }
                written.extend(drain(pty));
                written
            });
            write();
            finished.store(true, Ordering::SeqCst);
            reader.join().unwrap()
        })
    }

    #[test]
    fn shutdown_writes_the_normal_restore_then_cleanup_and_disarms() {
        let pty = test_pty::open();
        let cleanup = ExitCleanup {
            footer_top: Some(21),
            cursor_row: 6,
            rows: 24,
            sync_updates: false,
        };
        let written = drain_while(&pty, || {
            let mut terminal = test_pty::terminal(&pty);
            terminal.enable_raw_mode().unwrap();
            terminal.enter_interactive_mode().unwrap();
            terminal.shutdown(&cleanup);
            drop(terminal);
        });

        let written = String::from_utf8(written).unwrap();
        let expected = format!(
            "{}{}{}",
            super::super::interactive_mode_enable_sequence(false),
            NORMAL_EXIT_RESTORE,
            cleanup.sequence()
        );
        assert_eq!(written.replace("\r\n", "\n"), expected);
    }

    #[test]
    fn launch_rows_are_pushed_into_scrollback_from_the_bottom_row() {
        let pty = test_pty::open();
        let terminal = test_pty::terminal(&pty);
        terminal
            .push_launch_rows_into_scrollback(layout_24x80(), 2)
            .unwrap();
        terminal
            .push_launch_rows_into_scrollback(layout_24x80(), 0)
            .unwrap();
        let written = String::from_utf8(drain(&pty)).unwrap();
        assert_eq!(written.replace("\r\n", "\n"), "\x1b[24;1H\n\n");
    }

    #[test]
    fn signal_restoration_writes_the_abnormal_restore_and_leaves_raw_mode() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        terminal.enter_interactive_mode().unwrap();
        drain(&pty);
        terminal.restore_after_signal();
        let written = String::from_utf8(drain(&pty)).unwrap();
        assert_eq!(written.replace("\r\n", "\n"), ABNORMAL_EXIT_RESTORE);
        let restored = rustix::termios::tcgetattr(&pty.slave).unwrap();
        assert!(
            restored
                .local_modes
                .contains(rustix::termios::LocalModes::ICANON)
        );
        drop(terminal);
        assert!(drain(&pty).is_empty());
    }

    #[test]
    fn signal_restoration_discards_input_typed_for_the_session() {
        let pty = test_pty::open();
        let mut terminal = test_pty::terminal(&pty);
        terminal.enable_raw_mode().unwrap();
        test_pty::type_ahead(&pty, &terminal);
        terminal.restore_after_signal();
        assert_eq!(test_pty::unread_input(&pty), 0);
    }

    #[test]
    fn suspending_restores_the_terminal_stops_and_reenters_on_continue() {
        let cleanup = ExitCleanup {
            footer_top: Some(20),
            cursor_row: 1,
            rows: 24,
            sync_updates: false,
        };
        if test_pty::in_child() {
            let mut terminal = Terminal::open().unwrap();
            terminal.enable_raw_mode().unwrap();
            terminal.enter_interactive_mode().unwrap();
            let layout = terminal.suspend_to_job_control(&cleanup, 4).unwrap();
            assert!(!terminal.raw_mode_lost().unwrap());
            let rows = layout.map_or(0, |layout| layout.rows);
            terminal
                .write_all(format!("resumed {rows}\r\n").as_bytes())
                .unwrap();
            terminal.shutdown(&cleanup);
            return;
        }
        let mut session = test_pty::child_session(
            "terminal::app_lifecycle::tests::suspending_restores_the_terminal_stops_and_reenters_on_continue",
            &[("TERM", "xterm-256color")],
        );
        let restore = format!("{NORMAL_EXIT_RESTORE}{}", cleanup.sequence());
        let restore = restore.replace('\n', "\r\n");
        assert!(session.wait_until_stopped(test_pty::WAIT));
        session.resume().unwrap();
        let output = test_pty::wait_output(&session, b"resumed 24");
        let find = |needle: &[u8]| {
            output
                .windows(needle.len())
                .position(|window| window == needle)
                .unwrap()
        };
        let restored = find(restore.as_bytes()) + restore.len();
        let resumed = find(b"resumed 24");
        let enable = super::super::interactive_mode_enable_sequence(false).as_bytes();
        assert!(
            output[restored..resumed]
                .windows(enable.len())
                .any(|window| window == enable)
        );
        assert!(session.wait_exit(test_pty::WAIT).unwrap().success());
    }
}
