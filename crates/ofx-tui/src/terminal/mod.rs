mod app_lifecycle;
mod cursor_probe;
mod forwarded_bytes;
mod shell_runtime;
pub(crate) mod signal_pipe;
mod theme_detection;
mod theme_monitor;
mod theme_protocol;

use thiserror::Error;

pub(crate) use shell_runtime::Terminal;
pub(crate) use theme_monitor::{ThemeQuery, ThemeUpdate};

pub(crate) use forwarded_bytes::ForwardedBytes;
pub(crate) use theme_monitor::{FeedResult as ThemeMonitorFeed, Monitor as ThemeMonitor};

const INTERACTIVE_MODE_ENABLE_SEQUENCE: &str = "\x1b[>4;2m\x1b[>1u\x1b[?2004h\x1b[?7l";
const TMUX_INTERACTIVE_MODE_ENABLE_SEQUENCE: &str = "\x1b[>4;2m\x1b[?2004h\x1b[?7l";
pub(crate) const THEME_NOTIFICATION_ENABLE_SEQUENCE: &str = "\x1b[?2031h";
pub(crate) const THEME_COLOR_SCHEME_QUERY: &str = "\x1b[?996n";
pub(crate) const THEME_BACKGROUND_QUERY: &str = "\x1b]11;?\x1b\\";
pub(crate) const THEME_RESPONSE_FENCE_QUERY: &str = "\x1b[c";
pub(crate) const THEME_BACKGROUND_QUERY_WITH_FENCE: &str = "\x1b]11;?\x1b\\\x1b[c";
pub(crate) const CURSOR_POSITION_QUERY: &str = "\x1b[6n";

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("oh-fx requires an interactive terminal (TTY).")]
    NotATerminal,
    #[error("oh-fx cannot reopen its terminal for nonblocking output: {0}")]
    OutputUnavailable(std::io::Error),
    #[error("unable to read the terminal size")]
    UnableToReadTerminalSize,
    #[error("oh-fx needs at least 5 terminal rows.")]
    TerminalTooSmall,
    #[error("cursor position unavailable")]
    CursorPositionUnavailable,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<rustix::io::Errno> for TerminalError {
    fn from(errno: rustix::io::Errno) -> Self {
        Self::Io(errno.into())
    }
}

const MIN_LAYOUT_ROWS: u16 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) content_bottom: u16,
}

impl Layout {
    pub(crate) fn from_size(rows: u16, cols: u16, footer_rows: u16) -> Result<Self, TerminalError> {
        if cols == 0 || rows <= footer_rows || rows < MIN_LAYOUT_ROWS {
            return Err(TerminalError::TerminalTooSmall);
        }
        Ok(Self {
            rows,
            cols,
            content_bottom: rows - footer_rows,
        })
    }
}

pub(crate) fn interactive_mode_enable_sequence(tmux: bool) -> &'static str {
    if tmux {
        TMUX_INTERACTIVE_MODE_ENABLE_SEQUENCE
    } else {
        INTERACTIVE_MODE_ENABLE_SEQUENCE
    }
}

pub(crate) fn unstacked_interactive_mode_sequence() -> &'static str {
    TMUX_INTERACTIVE_MODE_ENABLE_SEQUENCE
}

pub(crate) fn move_cursor_sequence(row: u16, col: u16) -> String {
    format!("\x1b[{row};{col}H")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_cursor_sequence_formats_correctly() {
        assert_eq!(move_cursor_sequence(10, 5), "\x1b[10;5H");
    }

    #[test]
    fn move_cursor_sequence_row_1_col_1() {
        assert_eq!(move_cursor_sequence(1, 1), "\x1b[1;1H");
    }

    #[test]
    fn layout_from_size_derives_shared_native_and_wasm_terminal_geometry() {
        let layout = Layout::from_size(24, 80, 4).unwrap();
        assert_eq!(layout.rows, 24);
        assert_eq!(layout.cols, 80);
        assert_eq!(layout.content_bottom, 20);
        assert!(matches!(
            Layout::from_size(4, 80, 4),
            Err(TerminalError::TerminalTooSmall)
        ));
    }

    #[test]
    fn layout_from_size_rejects_tiny_terminals_without_underflow() {
        for (rows, cols, footer_rows) in [(3, 80, 1), (2, 80, 0), (1, 1, 0), (0, 0, 0), (10, 0, 4)]
        {
            assert!(matches!(
                Layout::from_size(rows, cols, footer_rows),
                Err(TerminalError::TerminalTooSmall)
            ));
        }
        let smallest = Layout::from_size(4, 1, 1).unwrap();
        assert_eq!(smallest.rows, 4);
        assert_eq!(smallest.content_bottom, 3);
    }

    #[test]
    fn interactive_mode_preserves_direct_terminal_keyboard_protocols() {
        assert_eq!(
            interactive_mode_enable_sequence(false),
            INTERACTIVE_MODE_ENABLE_SEQUENCE
        );
    }

    #[test]
    fn interactive_mode_leaves_kitty_keyboard_negotiation_to_tmux() {
        let sequence = interactive_mode_enable_sequence(true);
        assert_eq!(sequence, "\x1b[>4;2m\x1b[?2004h\x1b[?7l");
        assert!(!sequence.contains("\x1b[>1u"));
        assert!(sequence.contains("\x1b[>4;2m"));
        assert!(sequence.contains("\x1b[?2004h"));
        assert!(sequence.contains("\x1b[?7l"));
    }

    #[test]
    fn reasserting_interactive_mode_never_pushes_another_keyboard_level() {
        let sequence = unstacked_interactive_mode_sequence();
        assert!(!sequence.contains("\x1b[>1u"));
        assert_eq!(
            interactive_mode_enable_sequence(false).replace("\x1b[>1u", ""),
            sequence
        );
    }

    #[test]
    fn interactive_mode_leaves_native_terminal_scrollback_enabled() {
        for tmux in [false, true] {
            let sequence = interactive_mode_enable_sequence(tmux);
            assert!(!sequence.contains("\x1b[?1000h"));
            assert!(!sequence.contains("\x1b[?1002h"));
            assert!(!sequence.contains("\x1b[?1006h"));
        }
    }

    #[test]
    fn background_query_with_fence_appends_the_response_fence() {
        assert_eq!(
            THEME_BACKGROUND_QUERY_WITH_FENCE,
            format!("{THEME_BACKGROUND_QUERY}{THEME_RESPONSE_FENCE_QUERY}")
        );
    }
}
