use std::fmt::Write;

use crate::row_text::Row;

const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const ERASE_BELOW: &str = "\x1b[0m\x1b[J";
const ERASE_LINE_TAIL: &str = "\x1b[K";
const CLEAR_SCREEN_AND_HISTORY: &str = "\x1b[0m\x1b[2J\x1b[3J\x1b[H";

pub(crate) struct Frame<'a> {
    pub(crate) appended: &'a [Row],
    pub(crate) live: &'a [Row],
    pub(crate) cursor: Option<(usize, usize)>,
}

pub(crate) trait FrameSink {
    fn present(&mut self, frame: &Frame<'_>, out: &mut String);

    fn reset_screen(&mut self);

    fn screen_reset_pending(&self) -> bool;

    fn release_screen(&mut self);

    fn flush_queued(&mut self, out: &mut String);

    fn resize(&mut self, rows: u16, cols: u16);

    fn live_row(&self, index: usize) -> u16;

    fn cursor_row(&self) -> Option<u16>;
}

pub(crate) struct LiveRegionRenderer {
    rows: usize,
    cols: usize,
    sync_updates: bool,
    reset_sequence: String,
    before_frame: String,
    in_frame: String,
    top: usize,
    pinned: bool,
    padding: usize,
    drawn: Vec<String>,
    cursor: Option<(usize, usize)>,
    valid: bool,
}

impl LiveRegionRenderer {
    pub(crate) fn new(rows: u16, cols: u16, sync_updates: bool, reset_sequence: String) -> Self {
        Self {
            rows: usize::from(rows.max(1)),
            cols: usize::from(cols.max(1)),
            sync_updates,
            reset_sequence,
            before_frame: String::new(),
            in_frame: String::new(),
            top: 1,
            pinned: false,
            padding: 0,
            drawn: Vec::new(),
            cursor: None,
            valid: false,
        }
    }

    pub(crate) fn start_at(&mut self, row: usize) {
        self.top = row.clamp(1, self.rows);
    }

    fn restart_at_top(&mut self) {
        self.top = 1;
        self.pinned = false;
        self.padding = 0;
        self.drawn.clear();
        self.cursor = None;
        self.valid = false;
    }

    fn begin(&self, out: &mut String) {
        if self.sync_updates {
            out.push_str(SYNC_BEGIN);
        }
        out.push_str(HIDE_CURSOR);
    }

    fn end(&self, out: &mut String) {
        if let Some((row, col)) = self.cursor {
            move_to(out, row, col + 1);
        }
        if self.sync_updates {
            out.push_str(SYNC_END);
        }
        if self.cursor.is_some() {
            out.push_str(SHOW_CURSOR);
        }
    }

    fn target(&self, live: &[Row], padding: usize) -> Vec<(String, usize)> {
        let mut target = vec![(String::new(), 0); padding];
        target.extend(live.iter().map(|row| {
            let clipped = row.clipped(self.cols);
            (clipped.encode(), clipped.width())
        }));
        target
    }

    fn cursor_target(&self, frame: &Frame<'_>, live_len: usize) -> Option<(usize, usize)> {
        let (row, col) = frame.cursor?;
        let skipped = frame.live.len() - live_len;
        let row = row.checked_sub(skipped)?;
        Some((
            self.top + self.padding + row,
            col.min(self.cols.saturating_sub(1)),
        ))
    }

    fn append(&mut self, appended: &[Row], live: &[Row], out: &mut String) {
        let count = appended.len();
        let natural = live.len();
        let space_below = self.rows + 1 - self.top;
        let (top, padding) = if count + natural <= space_below {
            let top = self.top + count;
            let padding = if self.pinned {
                self.rows + 1 - top - natural
            } else {
                0
            };
            (top, padding)
        } else {
            (self.rows + 1 - natural, 0)
        };
        move_to(out, self.top, 1);
        out.push_str(ERASE_BELOW);
        let target = self.target(live, padding);
        let mut separator = "";
        for row in appended {
            out.push_str(separator);
            out.push_str(&row.clipped(self.cols).encode());
            separator = "\r\n";
        }
        for (line, _) in &target {
            out.push_str(separator);
            out.push_str(line);
            separator = "\r\n";
        }
        self.top = top;
        self.padding = padding;
        self.pinned = self.pinned || top + padding + natural > self.rows;
        self.drawn = target.into_iter().map(|(line, _)| line).collect();
    }

    fn redraw(&mut self, live: &[Row], out: &mut String) {
        let natural = live.len();
        let space_below = self.rows + 1 - self.top;
        let mut scrolled = false;
        if natural > space_below {
            let shift = natural - space_below;
            move_to(out, self.rows, 1);
            out.push_str(&"\n".repeat(shift));
            self.top -= shift;
            self.pinned = true;
            scrolled = true;
        }
        let padding = if self.pinned {
            self.rows + 1 - self.top - natural
        } else {
            0
        };
        let target = self.target(live, padding);
        for (index, (line, width)) in target.iter().enumerate() {
            if !scrolled && self.valid && self.drawn.get(index) == Some(line) {
                continue;
            }
            move_to(out, self.top + index, 1);
            out.push_str(line);
            if *width < self.cols {
                out.push_str(ERASE_LINE_TAIL);
            }
        }
        let below = self.top + target.len();
        if (target.len() < self.drawn.len() || !self.valid) && below <= self.rows {
            move_to(out, below, 1);
            out.push_str(ERASE_BELOW);
        }
        self.padding = padding;
        self.pinned = self.pinned || self.top + padding + natural > self.rows;
        self.drawn = target.into_iter().map(|(line, _)| line).collect();
    }
}

impl FrameSink for LiveRegionRenderer {
    fn present(&mut self, frame: &Frame<'_>, out: &mut String) {
        let live = &frame.live[frame.live.len().saturating_sub(self.rows)..];
        let mut body = String::new();
        if frame.appended.is_empty() {
            self.redraw(live, &mut body);
        } else {
            self.append(frame.appended, live, &mut body);
        }
        self.valid = true;
        let cursor = self.cursor_target(frame, live.len());
        if body.is_empty() && cursor == self.cursor && self.in_frame.is_empty() {
            return;
        }
        self.cursor = cursor;
        out.push_str(&self.before_frame);
        self.begin(out);
        out.push_str(&self.in_frame);
        out.push_str(&body);
        self.end(out);
        self.before_frame.clear();
        self.in_frame.clear();
    }

    fn reset_screen(&mut self) {
        self.before_frame.push_str(&self.reset_sequence);
        self.in_frame.push_str(CLEAR_SCREEN_AND_HISTORY);
        self.restart_at_top();
    }

    fn release_screen(&mut self) {
        move_to(&mut self.in_frame, self.top, 1);
        self.in_frame.push_str(ERASE_BELOW);
        if self.top > 1 {
            move_to(&mut self.in_frame, self.rows, 1);
            self.in_frame.push_str(&"\n".repeat(self.top - 1));
        }
        move_to(&mut self.in_frame, 1, 1);
        self.restart_at_top();
    }

    fn flush_queued(&mut self, out: &mut String) {
        out.push_str(&self.before_frame);
        out.push_str(&self.in_frame);
        self.before_frame.clear();
        self.in_frame.clear();
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = usize::from(rows.max(1));
        self.cols = usize::from(cols.max(1));
        self.valid = false;
    }

    fn screen_reset_pending(&self) -> bool {
        self.in_frame.contains(CLEAR_SCREEN_AND_HISTORY)
    }

    fn live_row(&self, index: usize) -> u16 {
        u16::try_from(self.top + self.padding + index).unwrap_or(u16::MAX)
    }

    fn cursor_row(&self) -> Option<u16> {
        let (row, _) = self.cursor.filter(|_| self.valid)?;
        u16::try_from(row).ok()
    }
}

fn move_to(out: &mut String, row: usize, col: usize) {
    let _ = write!(out, "\x1b[{row};{col}H");
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Screen {
        parser: vt100::Parser,
        renderer: LiveRegionRenderer,
    }

    impl Screen {
        fn new(rows: u16, cols: u16) -> Self {
            Self {
                parser: vt100::Parser::new(rows, cols, 200),
                renderer: LiveRegionRenderer::new(rows, cols, true, String::new()),
            }
        }

        fn present(
            &mut self,
            appended: &[&str],
            live: &[&str],
            cursor: Option<(usize, usize)>,
        ) -> String {
            let appended: Vec<Row> = appended.iter().map(|text| Row::plain(text)).collect();
            let live: Vec<Row> = live.iter().map(|text| Row::plain(text)).collect();
            let mut out = String::new();
            self.renderer.present(
                &Frame {
                    appended: &appended,
                    live: &live,
                    cursor,
                },
                &mut out,
            );
            self.parser.process(out.as_bytes());
            out
        }

        fn lines(&self) -> Vec<String> {
            self.parser
                .screen()
                .rows(0, self.parser.screen().size().1)
                .map(|row| row.trim_end().to_owned())
                .collect()
        }

        fn history(&mut self) -> Vec<String> {
            let rows = usize::from(self.parser.screen().size().0);
            self.parser.screen_mut().set_scrollback(usize::MAX);
            let depth = self.parser.screen().scrollback();
            let mut lines: Vec<String> = self
                .parser
                .screen()
                .rows(0, self.parser.screen().size().1)
                .take(depth.min(rows))
                .map(|row| row.trim_end().to_owned())
                .collect();
            lines.truncate(depth);
            self.parser.screen_mut().set_scrollback(0);
            lines
        }
    }

    #[test]
    fn the_live_region_starts_compact_below_the_transcript() {
        let mut screen = Screen::new(8, 20);
        screen.present(&["welcome", ""], &["┃ ", "", "auto · m"], Some((0, 2)));
        assert_eq!(
            screen.lines(),
            ["welcome", "", "┃", "", "auto · m", "", "", ""]
        );
        assert_eq!(screen.parser.screen().cursor_position(), (2, 2));
        assert!(!screen.parser.screen().hide_cursor());
    }

    #[test]
    fn the_live_region_can_start_below_the_launch_cursor() {
        let mut screen = Screen::new(8, 20);
        screen.renderer.start_at(4);
        screen.present(&[], &["┃ ", "", "auto · m"], Some((0, 2)));
        assert_eq!(screen.lines(), ["", "", "", "┃", "", "auto · m", "", ""]);
        assert_eq!(screen.parser.screen().cursor_position(), (3, 2));
        let mut clamped = Screen::new(8, 20);
        clamped.renderer.start_at(0);
        clamped.present(&[], &["top"], None);
        assert_eq!(clamped.lines()[0], "top");
        assert!(clamped.parser.screen().hide_cursor());
    }

    #[test]
    fn unchanged_frames_write_nothing_and_changed_rows_repaint_alone() {
        let mut screen = Screen::new(8, 20);
        screen.present(&["welcome", ""], &["┃ ", "", "auto · m"], Some((0, 2)));
        assert!(
            screen
                .present(&[], &["┃ ", "", "auto · m"], Some((0, 2)))
                .is_empty()
        );
        let typed = screen.present(&[], &["┃ hi", "", "auto · m"], Some((0, 4)));
        assert!(typed.contains("\x1b[3;1H"));
        assert!(!typed.contains("\x1b[5;1H"));
        assert!(typed.starts_with("\x1b[?2026h\x1b[?25l"));
        assert!(typed.ends_with("\x1b[3;5H\x1b[?2026l\x1b[?25h"));
        assert_eq!(screen.lines()[2], "┃ hi");
        let moved = screen.present(&[], &["┃ hi", "", "auto · m"], Some((0, 3)));
        assert_eq!(moved, "\x1b[?2026h\x1b[?25l\x1b[3;4H\x1b[?2026l\x1b[?25h");
    }

    #[test]
    fn appended_rows_push_the_live_region_down_and_then_into_scrollback() {
        let mut screen = Screen::new(6, 20);
        screen.present(&["welcome", ""], &["┃ ", "", "status"], None);
        screen.present(&["┃ one", "", "  reply"], &["", "┃ ", "", "status"], None);
        assert_eq!(screen.lines(), ["", "  reply", "", "┃", "", "status"]);
        assert_eq!(screen.history(), ["welcome", "", "┃ one"]);
        assert!(
            screen
                .present(&[], &["", "┃ ", "", "status"], None)
                .is_empty()
        );
    }

    #[test]
    fn a_pinned_region_pads_above_the_footer_when_it_shrinks() {
        let mut screen = Screen::new(6, 20);
        screen.present(
            &["a", "b", "c"],
            &["", "• Thinking", "", "┃ ", "status"],
            None,
        );
        assert_eq!(screen.lines(), ["c", "", "• Thinking", "", "┃", "status"]);
        assert_eq!(screen.history(), ["a", "b"]);
        screen.present(&[], &["┃ ", "status"], None);
        assert_eq!(screen.lines(), ["c", "", "", "", "┃", "status"]);
        screen.present(&["d"], &["", "┃ ", "status"], None);
        assert_eq!(screen.lines(), ["c", "d", "", "", "┃", "status"]);
        assert_eq!(screen.renderer.live_row(1), 5);
    }

    #[test]
    fn a_redraw_that_fills_the_screen_keeps_its_bottom_row() {
        let mut screen = Screen {
            parser: vt100::Parser::new(3, 20, 200),
            renderer: LiveRegionRenderer::new(3, 20, false, String::new()),
        };
        let live = ["first", "composer", "status"];
        screen.present(&[], &live, None);
        assert_eq!(screen.lines(), live);
        assert!(screen.present(&[], &live, None).is_empty());
        assert_eq!(screen.lines(), live);
    }

    #[test]
    fn a_wide_character_clipped_at_the_edge_leaves_no_stale_cell() {
        let mut screen = Screen::new(3, 4);
        screen.present(&[], &["abcd"], None);
        assert_eq!(screen.lines()[0], "abcd");
        screen.present(&[], &["abc界"], None);
        assert_eq!(screen.lines()[0], "abc");
    }

    #[test]
    fn full_width_rows_keep_their_last_cell() {
        let mut screen = Screen::new(4, 10);
        screen.present(&[], &["┃ ", "status"], None);
        screen.present(&[], &["┃ ", "0123456789"], None);
        assert_eq!(screen.lines()[1], "0123456789");
    }

    #[test]
    fn a_growing_live_region_scrolls_the_transcript_up() {
        let mut screen = Screen::new(5, 20);
        screen.present(&["a", "b"], &["┃ x", "status"], None);
        screen.present(&[], &["┃ x", "┃ y", "┃ z", "status"], None);
        assert_eq!(screen.lines(), ["b", "┃ x", "┃ y", "┃ z", "status"]);
        assert_eq!(screen.history(), ["a"]);
    }

    #[test]
    fn releasing_the_screen_moves_the_transcript_into_scrollback() {
        let mut screen = Screen::new(6, 20);
        screen.present(&["one", "two"], &["┃ ", "status"], None);
        screen.renderer.release_screen();
        let out = screen.present(&["welcome", ""], &["┃ ", "status"], None);
        assert!(out.starts_with(SYNC_BEGIN), "{out:?}");
        assert_eq!(screen.history(), ["one", "two"]);
        assert_eq!(screen.lines()[0], "welcome");
        assert_eq!(screen.lines()[3], "status");
    }

    #[test]
    fn resets_clear_the_screen_and_repaint_from_the_top() {
        let mut screen = Screen::new(6, 20);
        screen.present(&["one", "two"], &["┃ ", "status"], None);
        screen.renderer.resize(6, 10);
        screen.renderer.reset_screen();
        let out = screen.present(&["one", "two"], &["┃ abcdefghijklmnop", "status"], None);
        assert!(
            out.starts_with("\x1b[?2026h\x1b[?25l\x1b[0m\x1b[2J\x1b[3J\x1b[H"),
            "{out:?}"
        );
        assert_eq!(screen.lines()[2], "┃ abcdefgh");
    }

    #[test]
    fn a_full_terminal_reset_goes_out_before_the_synchronized_frame_that_clears() {
        let mut renderer = LiveRegionRenderer::new(6, 20, true, "\x1bc\x1b[?2004h".to_owned());
        let mut out = String::new();
        renderer.reset_screen();
        assert!(out.is_empty());
        renderer.present(
            &Frame {
                appended: &[Row::plain("one")],
                live: &[Row::plain("status")],
                cursor: None,
            },
            &mut out,
        );
        assert!(
            out.starts_with("\x1bc\x1b[?2004h\x1b[?2026h\x1b[?25l\x1b[0m\x1b[2J\x1b[3J\x1b[H"),
            "{out:?}"
        );
        let mut next = String::new();
        renderer.present(
            &Frame {
                appended: &[],
                live: &[Row::plain("status")],
                cursor: None,
            },
            &mut next,
        );
        assert!(next.is_empty(), "{next:?}");
    }
}
