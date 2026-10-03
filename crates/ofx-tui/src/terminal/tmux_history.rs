use std::env;
use std::ffi::OsString;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use super::Terminal;

const CLEAR_SCREEN_AND_HISTORY: &[u8] = b"\x1b[0m\x1b[2J\x1b[3J\x1b[H";
const SCREEN_CHECKS: usize = 5;
const SCREEN_CHECK_INTERVAL: Duration = Duration::from_millis(5);
const CLEAR_HISTORY_UNLESS_IN_MODE: &str = "#{?pane_in_mode,,clear-history -t #{pane_id}}";

pub(crate) struct TmuxHistory {
    program: OsString,
    pane: OsString,
}

impl TmuxHistory {
    pub(crate) fn detect() -> Option<Self> {
        env::var_os("TMUX")?;
        Some(Self {
            program: OsString::from("tmux"),
            pane: env::var_os("TMUX_PANE")?,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_program(program: impl Into<OsString>, pane: &str) -> Self {
        Self {
            program: program.into(),
            pane: OsString::from(pane),
        }
    }

    pub(crate) fn clear(&self, terminal: &Terminal) {
        if terminal.write_all(CLEAR_SCREEN_AND_HISTORY).is_err() {
            return;
        }
        self.wait_for_blank_pane();
        self.clear_history();
    }

    fn wait_for_blank_pane(&self) {
        for check in 1..=SCREEN_CHECKS {
            if self.pane_is_blank().unwrap_or(true) {
                return;
            }
            if check < SCREEN_CHECKS {
                thread::sleep(SCREEN_CHECK_INTERVAL);
            }
        }
    }

    fn pane_is_blank(&self) -> Option<bool> {
        let output = self
            .command(["capture-pane", "-p", "-t"])
            .stdout(Stdio::piped())
            .output()
            .ok()?;
        output.status.success().then(|| {
            output
                .stdout
                .iter()
                .all(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        })
    }

    fn clear_history(&self) {
        let _ = self
            .command(["run-shell", "-C", "-t"])
            .arg(CLEAR_HISTORY_UNLESS_IN_MODE)
            .stdout(Stdio::null())
            .status();
    }

    fn command<const N: usize>(&self, arguments: [&str; N]) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(arguments)
            .arg(&self.pane)
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        command
    }
}

#[cfg(test)]
pub(crate) mod fake_tmux {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    pub(crate) struct FakeTmux {
        directory: tempfile::TempDir,
    }

    impl FakeTmux {
        pub(crate) fn new(captures_before_blank: usize, capture_status: u8) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().display();
            let script = format!(
                "#!/bin/sh\n\
                 printf '%s|' \"$@\" >> '{root}/calls'\n\
                 printf '\\n' >> '{root}/calls'\n\
                 if [ \"$1\" = capture-pane ]; then\n\
                 count=$(($(cat '{root}/count' 2>/dev/null || echo 0) + 1))\n\
                 echo \"$count\" > '{root}/count'\n\
                 [ \"$count\" -le {captures_before_blank} ] && echo 'still drawing'\n\
                 exit {capture_status}\n\
                 fi\n"
            );
            let program = directory.path().join("tmux");
            fs::write(&program, script).unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
            Self { directory }
        }

        pub(crate) fn program(&self) -> PathBuf {
            self.directory.path().join("tmux")
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            fs::read_to_string(self.directory.path().join("calls"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake_tmux::FakeTmux;
    use super::*;
    use crate::terminal::test_pty;

    const CAPTURE: &str = "capture-pane|-p|-t|%7|";
    const CLEAR: &str = "run-shell|-C|-t|%7|#{?pane_in_mode,,clear-history -t #{pane_id}}|";

    fn cleared(fake: &FakeTmux) -> Vec<u8> {
        let pty = test_pty::open();
        let terminal = test_pty::terminal(&pty);
        TmuxHistory::with_program(fake.program(), "%7").clear(&terminal);
        test_pty::read_written(&pty)
    }

    #[test]
    fn the_screen_is_cleared_then_history_once_the_pane_shows_blank() {
        let fake = FakeTmux::new(2, 0);
        assert_eq!(cleared(&fake), CLEAR_SCREEN_AND_HISTORY);
        assert_eq!(fake.calls(), [CAPTURE, CAPTURE, CAPTURE, CLEAR]);
    }

    #[test]
    fn history_is_cleared_after_five_checks_of_a_pane_that_stays_drawn() {
        let fake = FakeTmux::new(100, 0);
        cleared(&fake);
        let mut expected = vec![CAPTURE; 5];
        expected.push(CLEAR);
        assert_eq!(fake.calls(), expected);
    }

    #[test]
    fn a_failed_capture_stops_waiting_but_still_clears_history() {
        let fake = FakeTmux::new(100, 1);
        cleared(&fake);
        assert_eq!(fake.calls(), [CAPTURE, CLEAR]);
    }

    #[test]
    fn a_missing_tmux_leaves_only_the_screen_clear() {
        let missing = TmuxHistory::with_program("/nonexistent/tmux", "%7");
        let pty = test_pty::open();
        missing.clear(&test_pty::terminal(&pty));
        assert_eq!(test_pty::read_written(&pty), CLEAR_SCREEN_AND_HISTORY);
    }
}
