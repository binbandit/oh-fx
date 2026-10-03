use ofx_text::display_unit_at;

const MAX_CSI_PARAMS: usize = 16;
const MAX_CSI_INTERMEDIATES: u8 = 2;
const MAX_STRING_BYTES: usize = 4096;
const MAX_SYNC_BYTES: usize = 1024 * 1024;
const MAX_POOL_ENTRIES: usize = 65_535;
const MAX_COMBINING_POOL_BYTES: usize = 4 * 1024 * 1024;
const MAX_CELL_TEXT_BYTES: usize = 64;
const MAX_RENDER_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const RENDER_CELL_BYTES: usize = 32;
const MAX_CELLS: usize = MAX_RENDER_SNAPSHOT_BYTES / RENDER_CELL_BYTES;
const DISPLAY_UNIT_WINDOW: usize = 256;
const TAB_INTERVAL: usize = 8;
const SYNC_RESET: &[u8] = b"\x1b[?2026l";
const REPLACEMENT: &[u8] = "\u{fffd}".as_bytes();
const REPLACEMENT_CODEPOINT: u32 = 0xfffd;
const ESC: u8 = 0x1b;
const CANCEL: u8 = 0x18;
const SUBSTITUTE: u8 = 0x1a;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GridError {
    #[error("InvalidGridSize")]
    InvalidGridSize,
    #[error("InvalidParserState")]
    InvalidParserState,
    #[error("SynchronizedUpdateTooLarge")]
    SynchronizedUpdateTooLarge,
    #[error("TooManyCsiParameters")]
    TooManyCsiParameters,
    #[error("TooManyCsiIntermediates")]
    TooManyCsiIntermediates,
    #[error("ControlStringTooLarge")]
    ControlStringTooLarge,
    #[error("CombiningPoolCapacityExceeded")]
    CombiningPoolCapacityExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cell {
    codepoint: u32,
    width: u8,
    suffix: u32,
}

const BLANK: Cell = Cell {
    codepoint: ' ' as u32,
    width: 1,
    suffix: 0,
};

const CONTINUATION: Cell = Cell {
    codepoint: 0,
    width: 0,
    suffix: 0,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParserState {
    Normal,
    Escape,
    Csi,
    Osc,
    Dcs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cursor {
    row: u16,
    col: u16,
    pending_wrap: bool,
}

const HOME: Cursor = Cursor {
    row: 1,
    col: 1,
    pending_wrap: false,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Modes {
    autowrap: bool,
    origin: bool,
    insert: bool,
}

const INITIAL_MODES: Modes = Modes {
    autowrap: true,
    origin: false,
    insert: false,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SavedCursor {
    cursor: Cursor,
    origin_mode: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Csi {
    params: [u16; MAX_CSI_PARAMS],
    count: usize,
    has_digit: bool,
    private: u8,
    intermediates: u8,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ControlString {
    saw_esc: bool,
    len: usize,
}

impl ControlString {
    fn push(&mut self) -> Result<(), GridError> {
        if self.len >= MAX_STRING_BYTES {
            return Err(GridError::ControlStringTooLarge);
        }
        self.len += 1;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SavedScreen {
    rows: u16,
    cols: u16,
    cells: Vec<Cell>,
    row_origin: u16,
    cursor: Cursor,
    modes: Modes,
    last_printable: Option<usize>,
    scroll_top: u16,
    scroll_bottom: u16,
    saved_cursor: Option<SavedCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grid {
    rows: u16,
    cols: u16,
    cells: Vec<Cell>,
    row_origin: u16,
    cursor: Cursor,
    modes: Modes,
    cursor_visible: bool,
    scroll_top: u16,
    scroll_bottom: u16,
    tab_stops: Vec<bool>,
    sync_active: bool,
    sync_buffer: Vec<u8>,
    state: ParserState,
    csi: Csi,
    osc: ControlString,
    dcs: ControlString,
    utf8_buffer: [u8; 4],
    utf8_len: usize,
    utf8_expected: usize,
    combining_pool: Vec<Vec<u8>>,
    combining_pool_bytes: usize,
    saved_normal_screen: Option<SavedScreen>,
    saved_cursor: Option<SavedCursor>,
    last_printable: Option<usize>,
}

impl Grid {
    pub fn new(cols: u16, rows: u16) -> Result<Self, GridError> {
        let count = cell_count(cols, rows)?;
        Ok(Self {
            rows,
            cols,
            cells: vec![BLANK; count],
            row_origin: 0,
            cursor: HOME,
            modes: INITIAL_MODES,
            cursor_visible: true,
            scroll_top: 1,
            scroll_bottom: rows,
            tab_stops: initial_tab_stops(cols),
            sync_active: false,
            sync_buffer: Vec::new(),
            state: ParserState::Normal,
            csi: Csi::default(),
            osc: ControlString::default(),
            dcs: ControlString::default(),
            utf8_buffer: [0; 4],
            utf8_len: 0,
            utf8_expected: 0,
            combining_pool: Vec::new(),
            combining_pool_bytes: 0,
            saved_normal_screen: None,
            saved_cursor: None,
            last_printable: None,
        })
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn cursor_row(&self) -> u16 {
        self.cursor.row
    }

    pub fn cursor_col(&self) -> u16 {
        self.cursor.col
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), GridError> {
        cell_count(cols, rows)?;
        if cols == self.cols && rows == self.rows {
            return Ok(());
        }
        self.cells = resized_cells(
            &self.cells,
            self.cols,
            self.rows,
            self.row_origin,
            cols,
            rows,
        );
        let mut stops = initial_tab_stops(cols);
        let kept = stops.len().min(self.tab_stops.len());
        stops[..kept].copy_from_slice(&self.tab_stops[..kept]);
        self.tab_stops = stops;
        self.cols = cols;
        self.rows = rows;
        self.row_origin = 0;
        self.cursor.row = self.cursor.row.min(rows);
        self.cursor.col = self.cursor.col.min(cols);
        self.last_printable = None;
        self.scroll_top = 1;
        self.scroll_bottom = rows;
        self.modes.origin = false;
        if let Some(saved) = &mut self.saved_normal_screen {
            saved.cells = resized_cells(
                &saved.cells,
                saved.cols,
                saved.rows,
                saved.row_origin,
                cols,
                rows,
            );
            saved.rows = rows;
            saved.cols = cols;
            saved.row_origin = 0;
            saved.cursor.row = saved.cursor.row.min(rows);
            saved.cursor.col = saved.cursor.col.min(cols);
            saved.last_printable = None;
            saved.scroll_top = 1;
            saved.scroll_bottom = rows;
            saved.modes.origin = false;
        }
        Ok(())
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), GridError> {
        let mut remaining = bytes;
        while !remaining.is_empty() {
            if !self.sync_active {
                let consumed = self.feed_direct(remaining, true)?;
                remaining = &remaining[consumed..];
                if consumed == 0 {
                    return Err(GridError::InvalidParserState);
                }
                if !self.sync_active || remaining.is_empty() {
                    continue;
                }
            }
            if self.sync_buffer.len() >= MAX_SYNC_BYTES {
                return Err(GridError::SynchronizedUpdateTooLarge);
            }
            self.sync_buffer.push(remaining[0]);
            remaining = &remaining[1..];
            if !self.sync_buffer.ends_with(SYNC_RESET) {
                continue;
            }
            let kept = self.sync_buffer.len() - SYNC_RESET.len();
            self.sync_buffer.truncate(kept);
            let buffered = std::mem::take(&mut self.sync_buffer);
            self.sync_active = false;
            if self.feed_direct(&buffered, false)? != buffered.len() {
                return Err(GridError::InvalidParserState);
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for row in 1..=self.rows {
            out.push(b'|');
            self.row_text(row, &mut out);
            out.extend_from_slice(b"|\n");
        }
        out
    }

    fn row_text(&self, row: u16, out: &mut Vec<u8>) {
        let base = self.row_base(row);
        for cell in &self.cells[base..base + usize::from(self.cols)] {
            if cell.width == 0 {
                continue;
            }
            let character = match cell.codepoint {
                0 => ' ',
                codepoint => char::from_u32(codepoint).unwrap_or('\u{fffd}'),
            };
            let mut encoded = [0_u8; 4];
            out.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
            out.extend_from_slice(self.combining_suffix(cell.suffix));
        }
    }

    fn combining_suffix(&self, id: u32) -> &[u8] {
        usize::try_from(id)
            .ok()
            .and_then(|id| id.checked_sub(1))
            .and_then(|index| self.combining_pool.get(index))
            .map_or(&[], Vec::as_slice)
    }

    fn row_base(&self, row: u16) -> usize {
        physical_row(self.row_origin, row - 1, self.rows) * usize::from(self.cols)
    }

    fn cell_index(&self, row: u16, col: u16) -> usize {
        self.row_base(row) + usize::from(col) - 1
    }

    fn physical_offset(&self, offset: usize) -> usize {
        let cols = usize::from(self.cols);
        let logical_row = u16::try_from(offset / cols).unwrap_or(u16::MAX);
        physical_row(self.row_origin, logical_row, self.rows) * cols + offset % cols
    }

    fn feed_direct(&mut self, bytes: &[u8], stop_on_sync_start: bool) -> Result<usize, GridError> {
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if byte == CANCEL || byte == SUBSTITUTE {
                self.cancel_control_sequence();
                index += 1;
                continue;
            }
            match self.state {
                ParserState::Normal => {
                    if self.utf8_len != 0 {
                        index += self.complete_pending_utf8(&bytes[index..])?;
                        continue;
                    }
                    if byte == ESC {
                        self.last_printable = None;
                        self.state = ParserState::Escape;
                        index += 1;
                        continue;
                    }
                    let expected = utf8_sequence_len(byte);
                    if expected > 1 && index + expected > bytes.len() {
                        let tail = &bytes[index..];
                        self.utf8_buffer[..tail.len()].copy_from_slice(tail);
                        self.utf8_len = tail.len();
                        self.utf8_expected = expected;
                        return Ok(bytes.len());
                    }
                    index += self.write_byte(bytes, index)?;
                }
                ParserState::Escape => {
                    self.dispatch_escape(byte);
                    index += 1;
                }
                ParserState::Csi => {
                    index += 1;
                    if self.feed_csi(byte)? && stop_on_sync_start && self.sync_active {
                        return Ok(index);
                    }
                }
                ParserState::Osc => {
                    if byte == 0x07 || (self.osc.saw_esc && byte == b'\\') {
                        self.cancel_control_sequence();
                    } else if byte == ESC {
                        self.osc.saw_esc = true;
                    } else {
                        if self.osc.saw_esc {
                            self.osc.push()?;
                            self.osc.saw_esc = false;
                        }
                        self.osc.push()?;
                    }
                    index += 1;
                }
                ParserState::Dcs => {
                    if byte == ESC {
                        self.dcs.saw_esc = true;
                    } else if self.dcs.saw_esc && byte == b'\\' {
                        self.cancel_control_sequence();
                    } else {
                        if self.dcs.saw_esc {
                            self.dcs.push()?;
                            self.dcs.saw_esc = false;
                        }
                        self.dcs.push()?;
                    }
                    index += 1;
                }
            }
        }
        Ok(index)
    }

    fn feed_csi(&mut self, byte: u8) -> Result<bool, GridError> {
        if matches!(byte, b'?' | b'>' | b'<' | b'=')
            && self.csi.count == 0
            && !self.csi.has_digit
            && self.csi.intermediates == 0
        {
            if self.csi.private == 0 {
                self.csi.private = byte;
            }
            return Ok(false);
        }
        if byte.is_ascii_digit() {
            let slot = &mut self.csi.params[self.csi.count];
            *slot = slot
                .saturating_mul(10)
                .saturating_add(u16::from(byte - b'0'));
            self.csi.has_digit = true;
            return Ok(false);
        }
        if byte == b';' || byte == b':' {
            if self.csi.count + 1 >= MAX_CSI_PARAMS {
                return Err(GridError::TooManyCsiParameters);
            }
            self.csi.count += 1;
            self.csi.has_digit = false;
            return Ok(false);
        }
        if (0x20..=0x2f).contains(&byte) {
            if self.csi.intermediates >= MAX_CSI_INTERMEDIATES {
                return Err(GridError::TooManyCsiIntermediates);
            }
            self.csi.intermediates += 1;
            return Ok(false);
        }
        if !(0x40..=0x7e).contains(&byte) {
            self.cancel_control_sequence();
            return Ok(false);
        }
        if self.csi.has_digit || self.csi.count > 0 {
            self.csi.count += 1;
        }
        self.dispatch_csi(byte);
        self.state = ParserState::Normal;
        Ok(true)
    }

    fn dispatch_escape(&mut self, byte: u8) {
        self.state = ParserState::Normal;
        match byte {
            b'[' => {
                self.csi = Csi::default();
                self.state = ParserState::Csi;
            }
            b']' => {
                self.osc = ControlString::default();
                self.state = ParserState::Osc;
            }
            b'P' => {
                self.dcs = ControlString::default();
                self.state = ParserState::Dcs;
            }
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => {
                self.cursor.pending_wrap = false;
                self.advance_row_or_scroll();
            }
            b'E' => {
                self.cursor.pending_wrap = false;
                self.cursor.col = 1;
                self.advance_row_or_scroll();
            }
            b'M' => {
                self.cursor.pending_wrap = false;
                self.reverse_index();
            }
            b'H' => self.tab_stops[usize::from(self.cursor.col) - 1] = true,
            b'c' => self.reset_terminal(),
            _ => {}
        }
    }

    fn cancel_control_sequence(&mut self) {
        self.state = ParserState::Normal;
        self.csi = Csi::default();
        self.osc = ControlString::default();
        self.dcs = ControlString::default();
    }

    fn complete_pending_utf8(&mut self, bytes: &[u8]) -> Result<usize, GridError> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if bytes[0] & 0xc0 != 0x80 {
            self.utf8_len = 0;
            self.utf8_expected = 0;
            self.write_byte(REPLACEMENT, 0)?;
            return Ok(0);
        }
        let count = bytes.len().min(self.utf8_expected - self.utf8_len);
        self.utf8_buffer[self.utf8_len..self.utf8_len + count].copy_from_slice(&bytes[..count]);
        self.utf8_len += count;
        if self.utf8_len != self.utf8_expected {
            return Ok(count);
        }
        let buffer = self.utf8_buffer;
        let complete = &buffer[..self.utf8_len];
        self.utf8_len = 0;
        self.utf8_expected = 0;
        if std::str::from_utf8(complete).is_ok() {
            self.write_byte(complete, 0)?;
        } else {
            self.write_byte(REPLACEMENT, 0)?;
        }
        Ok(count)
    }

    fn write_byte(&mut self, bytes: &[u8], start: usize) -> Result<usize, GridError> {
        let byte = bytes[start];
        match byte {
            b'\n' => {
                self.last_printable = None;
                self.cursor.pending_wrap = false;
                self.cursor.col = 1;
                self.advance_row_or_scroll();
                return Ok(1);
            }
            b'\r' => {
                self.last_printable = None;
                self.cursor.pending_wrap = false;
                self.cursor.col = 1;
                return Ok(1);
            }
            0x08 => {
                self.last_printable = None;
                self.cursor.pending_wrap = false;
                self.cursor.col = self.cursor.col.saturating_sub(1).max(1);
                return Ok(1);
            }
            b'\t' => {
                self.last_printable = None;
                self.cursor.pending_wrap = false;
                self.move_tabs_forward(1);
                return Ok(1);
            }
            0..0x20 => {
                self.last_printable = None;
                return Ok(1);
            }
            _ => {}
        }

        let (consumed, width) = display_unit(bytes, start);
        let (codepoint, decoded_len) = decode_utf8(bytes, start);
        if width == 0 {
            if let Some(index) = self.last_printable {
                self.append_combining_suffix(index, &bytes[start..start + consumed])?;
            }
            return Ok(consumed);
        }
        self.last_printable = None;
        if self.cursor.pending_wrap && self.modes.autowrap {
            self.cursor.col = 1;
            self.advance_row_or_scroll();
        }
        self.cursor.pending_wrap = false;

        let width_cols = u16::from(width);
        let cols = u32::from(self.cols);
        if u32::from(self.cursor.col) + u32::from(width_cols) - 1 > cols {
            if self.modes.autowrap {
                self.cursor.col = 1;
                self.advance_row_or_scroll();
            } else if self.cols >= width_cols {
                self.cursor.col = self.cols - width_cols + 1;
            } else {
                return Ok(consumed);
            }
        }

        if self.modes.insert {
            self.insert_cells(width_cols);
        }
        let index = self.cell_index(self.cursor.row, self.cursor.col);
        self.clear_wide_glyph_at(self.cursor.row, self.cursor.col);
        if width == 2 {
            self.clear_wide_glyph_at(self.cursor.row, self.cursor.col + 1);
        }
        self.cells[index] = Cell {
            codepoint,
            width,
            suffix: 0,
        };
        if decoded_len < consumed {
            self.append_combining_suffix(index, &bytes[start + decoded_len..start + consumed])?;
        }
        self.last_printable = Some(index);
        if width == 2 && self.cursor.col < self.cols {
            self.cells[index + 1] = CONTINUATION;
        }

        if u32::from(self.cursor.col) + u32::from(width_cols) <= cols {
            self.cursor.col += width_cols;
        } else {
            self.cursor.col = self.cols;
            if self.modes.autowrap {
                self.cursor.pending_wrap = true;
            }
        }
        Ok(consumed)
    }

    fn append_combining_suffix(&mut self, index: usize, bytes: &[u8]) -> Result<(), GridError> {
        let existing = self.combining_suffix(self.cells[index].suffix).to_vec();
        let combined_len = existing.len() + bytes.len();
        let reused = self.combining_pool.iter().position(|candidate| {
            candidate.len() == combined_len
                && candidate.starts_with(&existing)
                && &candidate[existing.len()..] == bytes
        });
        if let Some(position) = reused {
            self.cells[index].suffix = pool_id(position + 1);
            return Ok(());
        }
        if combined_len > MAX_CELL_TEXT_BYTES
            || self.combining_pool.len() >= MAX_POOL_ENTRIES
            || self.combining_pool_bytes > MAX_COMBINING_POOL_BYTES - combined_len
        {
            return Err(GridError::CombiningPoolCapacityExceeded);
        }
        let mut combined = existing;
        combined.extend_from_slice(bytes);
        self.combining_pool_bytes += combined.len();
        self.combining_pool.push(combined);
        self.cells[index].suffix = pool_id(self.combining_pool.len());
        Ok(())
    }

    fn advance_row_or_scroll(&mut self) {
        if self.cursor.row == self.scroll_bottom {
            self.scroll_up(self.scroll_top, self.scroll_bottom, 1);
        } else if self.cursor.row < self.rows {
            self.cursor.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor.row == self.scroll_top {
            self.scroll_down(self.scroll_top, self.scroll_bottom, 1);
        } else if self.cursor.row > 1 {
            self.cursor.row -= 1;
        }
    }

    fn fill_row(&mut self, row: u16) {
        let base = self.row_base(row);
        self.cells[base..base + usize::from(self.cols)].fill(BLANK);
    }

    fn copy_row(&mut self, destination: u16, source: u16) {
        let from = self.row_base(source);
        let to = self.row_base(destination);
        self.cells
            .copy_within(from..from + usize::from(self.cols), to);
    }

    fn scroll_up(&mut self, top: u16, bottom: u16, requested: u16) {
        if top == 0 || bottom < top || bottom > self.rows {
            return;
        }
        let count = requested.min(bottom - top + 1);
        if count == 0 {
            return;
        }
        if top == 1 && bottom == self.rows && count == 1 {
            self.row_origin += 1;
            if self.row_origin == self.rows {
                self.row_origin = 0;
            }
            self.fill_row(self.rows);
            return;
        }
        let kept = bottom - top + 1 - count;
        for offset in 0..kept {
            self.copy_row(top + offset, top + offset + count);
        }
        for offset in kept..=bottom - top {
            self.fill_row(top + offset);
        }
    }

    fn scroll_down(&mut self, top: u16, bottom: u16, requested: u16) {
        if top == 0 || bottom < top || bottom > self.rows {
            return;
        }
        let count = requested.min(bottom - top + 1);
        if count == 0 {
            return;
        }
        let kept = bottom - top + 1 - count;
        for offset in 0..kept {
            self.copy_row(bottom - offset, bottom - offset - count);
        }
        for offset in 0..count {
            self.fill_row(top + offset);
        }
    }

    fn param(&self, index: usize, default: u16) -> u16 {
        match self.raw_param(index, 0) {
            0 => default,
            value => value,
        }
    }

    fn raw_param(&self, index: usize, default: u16) -> u16 {
        if index < self.csi.count {
            self.csi.params[index]
        } else {
            default
        }
    }

    fn dispatch_csi(&mut self, final_byte: u8) {
        match final_byte {
            b'H' | b'f' => self.position_cursor(self.param(0, 1), self.param(1, 1)),
            b'A' => {
                self.cursor.row = clamp_sub(self.cursor.row, self.param(0, 1), self.cursor_top());
                self.cursor.pending_wrap = false;
            }
            b'B' | b'e' => {
                self.cursor.row = clamp(
                    self.cursor.row.saturating_add(self.param(0, 1)),
                    self.cursor_top(),
                    self.cursor_bottom(),
                );
                self.cursor.pending_wrap = false;
            }
            b'C' | b'a' => {
                self.cursor.col = clamp(
                    self.cursor.col.saturating_add(self.param(0, 1)),
                    1,
                    self.cols,
                );
                self.cursor.pending_wrap = false;
            }
            b'D' => {
                self.cursor.col = clamp_sub(self.cursor.col, self.param(0, 1), 1);
                self.cursor.pending_wrap = false;
            }
            b'E' => {
                self.cursor.row = clamp(
                    self.cursor.row.saturating_add(self.param(0, 1)),
                    self.cursor_top(),
                    self.cursor_bottom(),
                );
                self.cursor.col = 1;
                self.cursor.pending_wrap = false;
            }
            b'F' => {
                self.cursor.row = clamp_sub(self.cursor.row, self.param(0, 1), self.cursor_top());
                self.cursor.col = 1;
                self.cursor.pending_wrap = false;
            }
            b'G' | b'`' => {
                self.cursor.col = clamp(self.param(0, 1), 1, self.cols);
                self.cursor.pending_wrap = false;
            }
            b'd' => {
                self.cursor.row = clamp(self.param(0, 1), 1, self.rows);
                self.cursor.pending_wrap = false;
            }
            b'J' => self.erase_display(self.raw_param(0, 0)),
            b'K' => self.erase_line(self.raw_param(0, 0)),
            b'@' => self.insert_cells(self.param(0, 1)),
            b'P' => self.delete_cells(self.param(0, 1)),
            b'X' => self.erase_cells(self.param(0, 1)),
            b'L' => self.insert_lines(self.param(0, 1)),
            b'M' => self.delete_lines(self.param(0, 1)),
            b'S' => self.scroll_up(self.scroll_top, self.scroll_bottom, self.param(0, 1)),
            b'T' => self.scroll_down(self.scroll_top, self.scroll_bottom, self.param(0, 1)),
            b'I' => self.move_tabs_forward(self.param(0, 1)),
            b'Z' => self.move_tabs_backward(self.param(0, 1)),
            b'g' => self.clear_tab_stops(self.raw_param(0, 0)),
            b'h' | b'l' => self.set_mode(final_byte == b'h'),
            b'r' => self.set_scroll_region(),
            b's' => self.save_cursor(),
            b'u' if self.csi.private == 0 => self.restore_cursor(),
            _ => {}
        }
    }

    fn cursor_top(&self) -> u16 {
        if self.modes.origin {
            self.scroll_top
        } else {
            1
        }
    }

    fn cursor_bottom(&self) -> u16 {
        if self.modes.origin {
            self.scroll_bottom
        } else {
            self.rows
        }
    }

    fn position_cursor(&mut self, row: u16, col: u16) {
        let top = self.cursor_top();
        let absolute = if self.modes.origin {
            top.saturating_add(row.saturating_sub(1))
        } else {
            row
        };
        self.cursor.row = clamp(absolute, top, self.cursor_bottom());
        self.cursor.col = clamp(col, 1, self.cols);
        self.cursor.pending_wrap = false;
    }

    fn save_cursor(&mut self) {
        self.saved_cursor = Some(SavedCursor {
            cursor: self.cursor,
            origin_mode: self.modes.origin,
        });
    }

    fn restore_cursor(&mut self) {
        let Some(saved) = self.saved_cursor else {
            return;
        };
        self.cursor = Cursor {
            row: clamp(saved.cursor.row, 1, self.rows),
            col: clamp(saved.cursor.col, 1, self.cols),
            pending_wrap: saved.cursor.pending_wrap,
        };
        self.modes.origin = saved.origin_mode;
    }

    fn set_scroll_region(&mut self) {
        if self.csi.private != 0 {
            return;
        }
        let top = clamp(self.param(0, 1), 1, self.rows);
        let bottom = clamp(self.param(1, self.rows), 1, self.rows);
        if top >= bottom {
            return;
        }
        self.scroll_top = top;
        self.scroll_bottom = bottom;
        self.position_cursor(1, 1);
    }

    fn row_span(&self, requested: u16) -> Option<(usize, usize, usize)> {
        let count = requested.min(self.cols - self.cursor.col + 1);
        if count == 0 {
            return None;
        }
        let base = self.row_base(self.cursor.row);
        Some((
            base,
            base + usize::from(self.cursor.col) - 1,
            usize::from(count),
        ))
    }

    fn insert_cells(&mut self, requested: u16) {
        let Some((base, start, count)) = self.row_span(requested) else {
            return;
        };
        let end = base + usize::from(self.cols);
        self.cells.copy_within(start..end - count, start + count);
        self.cells[start..start + count].fill(BLANK);
        repair_wide_cells(&mut self.cells[base..end], self.cols, 1);
        self.cursor.pending_wrap = false;
        self.last_printable = None;
    }

    fn delete_cells(&mut self, requested: u16) {
        let Some((base, start, count)) = self.row_span(requested) else {
            return;
        };
        let end = base + usize::from(self.cols);
        self.cells.copy_within(start + count..end, start);
        self.cells[end - count..end].fill(BLANK);
        repair_wide_cells(&mut self.cells[base..end], self.cols, 1);
        self.cursor.pending_wrap = false;
        self.last_printable = None;
    }

    fn erase_cells(&mut self, requested: u16) {
        let count = requested.min(self.cols - self.cursor.col + 1);
        if count == 0 {
            return;
        }
        let start = self.logical_offset(self.cursor.row, self.cursor.col - 1);
        self.erase_range(start, start + usize::from(count));
        self.cursor.pending_wrap = false;
        self.last_printable = None;
    }

    fn insert_lines(&mut self, requested: u16) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bottom {
            return;
        }
        self.scroll_down(self.cursor.row, self.scroll_bottom, requested);
        self.cursor.pending_wrap = false;
        self.last_printable = None;
    }

    fn delete_lines(&mut self, requested: u16) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bottom {
            return;
        }
        self.scroll_up(self.cursor.row, self.scroll_bottom, requested);
        self.cursor.pending_wrap = false;
        self.last_printable = None;
    }

    fn move_tabs_forward(&mut self, requested: u16) {
        for _ in 0..requested {
            let mut column = self.cursor.col.saturating_add(1);
            while column < self.cols && !self.tab_stops[usize::from(column) - 1] {
                column += 1;
            }
            self.cursor.col = column.min(self.cols);
        }
        self.cursor.pending_wrap = false;
    }

    fn move_tabs_backward(&mut self, requested: u16) {
        for _ in 0..requested {
            if self.cursor.col <= 1 {
                break;
            }
            let mut column = self.cursor.col - 1;
            while column > 1 && !self.tab_stops[usize::from(column) - 1] {
                column -= 1;
            }
            self.cursor.col = column;
        }
        self.cursor.pending_wrap = false;
    }

    fn clear_tab_stops(&mut self, mode: u16) {
        match mode {
            0 => self.tab_stops[usize::from(self.cursor.col) - 1] = false,
            3 => self.tab_stops.fill(false),
            _ => {}
        }
    }

    fn clear_wide_glyph_at(&mut self, row: u16, col: u16) {
        if row == 0 || row > self.rows || col == 0 || col > self.cols {
            return;
        }
        let index = self.cell_index(row, col);
        match self.cells[index].width {
            0 => {
                if col > 1 && self.cells[index - 1].width == 2 {
                    self.cells[index - 1] = BLANK;
                }
                self.cells[index] = BLANK;
            }
            2 => {
                self.cells[index] = BLANK;
                if col < self.cols && self.cells[index + 1].width == 0 {
                    self.cells[index + 1] = BLANK;
                }
            }
            _ => {}
        }
    }

    fn logical_offset(&self, row: u16, col_offset: u16) -> usize {
        (usize::from(row) - 1) * usize::from(self.cols) + usize::from(col_offset)
    }

    fn erase_range(&mut self, start: usize, end: usize) {
        let cols = usize::from(self.cols);
        let mut start = start;
        let mut end = end;
        if start < end
            && !start.is_multiple_of(cols)
            && self.cells[self.physical_offset(start)].width == 0
        {
            start -= 1;
        }
        if end > start
            && end < self.cells.len()
            && !end.is_multiple_of(cols)
            && self.cells[self.physical_offset(end - 1)].width == 2
        {
            end += 1;
        }
        let mut logical = start;
        while logical < end {
            let chunk = (cols - logical % cols).min(end - logical);
            let physical = self.physical_offset(logical);
            self.cells[physical..physical + chunk].fill(BLANK);
            logical += chunk;
        }
    }

    fn erase_display(&mut self, mode: u16) {
        let total = self.cells.len();
        match mode {
            0 => {
                let start = self.logical_offset(self.cursor.row, self.cursor.col - 1);
                self.erase_range(start, total);
            }
            1 => {
                let end = self.logical_offset(self.cursor.row, self.cursor.col);
                self.erase_range(0, end);
            }
            2 => self.erase_range(0, total),
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let row_start = self.logical_offset(self.cursor.row, 0);
        let row_end = row_start + usize::from(self.cols);
        match mode {
            0 => self.erase_range(
                self.logical_offset(self.cursor.row, self.cursor.col - 1),
                row_end,
            ),
            1 => self.erase_range(
                row_start,
                self.logical_offset(self.cursor.row, self.cursor.col),
            ),
            2 => self.erase_range(row_start, row_end),
            _ => {}
        }
    }

    fn set_mode(&mut self, set: bool) {
        let params = self.csi.params;
        let params = &params[..self.csi.count];
        if self.csi.private == 0 {
            if params.contains(&4) {
                self.modes.insert = set;
            }
            return;
        }
        if self.csi.private != b'?' {
            return;
        }
        for &mode in params {
            match mode {
                6 => {
                    self.modes.origin = set;
                    self.position_cursor(1, 1);
                }
                7 => self.modes.autowrap = set,
                25 => self.cursor_visible = set,
                47 | 1047 | 1049 if set => self.enter_alternate_screen(),
                47 | 1047 | 1049 => self.leave_alternate_screen(),
                2026 => self.sync_active = set,
                _ => {}
            }
        }
    }

    fn reset_terminal(&mut self) {
        if self.saved_normal_screen.is_some() {
            self.leave_alternate_screen();
        }
        self.cells.fill(BLANK);
        self.row_origin = 0;
        self.cursor = HOME;
        self.modes = INITIAL_MODES;
        self.cursor_visible = true;
        self.scroll_top = 1;
        self.scroll_bottom = self.rows;
        self.sync_active = false;
        self.sync_buffer.clear();
        self.combining_pool.clear();
        self.combining_pool_bytes = 0;
        self.saved_cursor = None;
        self.last_printable = None;
        self.utf8_len = 0;
        self.utf8_expected = 0;
        self.tab_stops = initial_tab_stops(self.cols);
        self.cancel_control_sequence();
    }

    fn enter_alternate_screen(&mut self) {
        if self.saved_normal_screen.is_some() {
            return;
        }
        let alternate = vec![BLANK; self.cells.len()];
        self.saved_normal_screen = Some(SavedScreen {
            rows: self.rows,
            cols: self.cols,
            cells: std::mem::replace(&mut self.cells, alternate),
            row_origin: self.row_origin,
            cursor: self.cursor,
            modes: self.modes,
            last_printable: self.last_printable,
            scroll_top: self.scroll_top,
            scroll_bottom: self.scroll_bottom,
            saved_cursor: self.saved_cursor,
        });
        self.row_origin = 0;
        self.cursor = HOME;
        self.modes = INITIAL_MODES;
        self.scroll_top = 1;
        self.scroll_bottom = self.rows;
        self.last_printable = None;
        self.saved_cursor = None;
    }

    fn leave_alternate_screen(&mut self) {
        let Some(saved) = self.saved_normal_screen.take() else {
            return;
        };
        self.rows = saved.rows;
        self.cols = saved.cols;
        self.cells = saved.cells;
        self.row_origin = saved.row_origin;
        self.cursor = saved.cursor;
        self.modes = saved.modes;
        self.last_printable = saved.last_printable;
        self.scroll_top = saved.scroll_top;
        self.scroll_bottom = saved.scroll_bottom;
        self.saved_cursor = saved.saved_cursor;
    }
}

fn cell_count(cols: u16, rows: u16) -> Result<usize, GridError> {
    let count = usize::from(cols) * usize::from(rows);
    if count == 0 || count > MAX_CELLS {
        return Err(GridError::InvalidGridSize);
    }
    Ok(count)
}

fn initial_tab_stops(cols: u16) -> Vec<bool> {
    (0..usize::from(cols))
        .map(|index| index != 0 && index % TAB_INTERVAL == 0)
        .collect()
}

fn resized_cells(
    source: &[Cell],
    source_cols: u16,
    source_rows: u16,
    source_origin: u16,
    cols: u16,
    rows: u16,
) -> Vec<Cell> {
    let mut cells = vec![BLANK; usize::from(cols) * usize::from(rows)];
    let copy_cols = usize::from(source_cols.min(cols));
    for row in 0..source_rows.min(rows) {
        let from = physical_row(source_origin, row, source_rows) * usize::from(source_cols);
        let to = usize::from(row) * usize::from(cols);
        cells[to..to + copy_cols].copy_from_slice(&source[from..from + copy_cols]);
    }
    repair_wide_cells(&mut cells, cols, rows);
    cells
}

fn repair_wide_cells(cells: &mut [Cell], cols: u16, rows: u16) {
    let cols = usize::from(cols);
    for row in 0..usize::from(rows) {
        let base = row * cols;
        for col in 0..cols {
            let index = base + col;
            let current = cells[index];
            let broken = match current.width {
                0 => {
                    col == 0
                        || current.codepoint != 0
                        || current.suffix != 0
                        || cells[index - 1].width != 2
                }
                1 => false,
                2 => {
                    let next = cells.get(index + 1).filter(|_| col + 1 < cols);
                    next.is_none_or(|next| {
                        next.width != 0 || next.codepoint != 0 || next.suffix != 0
                    })
                }
                _ => true,
            };
            if broken {
                cells[index] = BLANK;
            }
        }
    }
}

fn physical_row(origin: u16, logical_row: u16, rows: u16) -> usize {
    let index = usize::from(origin) + usize::from(logical_row);
    let rows = usize::from(rows);
    if index < rows { index } else { index - rows }
}

fn clamp(value: u16, low: u16, high: u16) -> u16 {
    if high < low {
        return low;
    }
    value.clamp(low, high)
}

fn clamp_sub(value: u16, subtrahend: u16, low: u16) -> u16 {
    if subtrahend >= value {
        return low;
    }
    (value - subtrahend).max(low)
}

fn pool_id(position: usize) -> u32 {
    u32::try_from(position).unwrap_or(u32::MAX)
}

fn utf8_sequence_len(byte: u8) -> usize {
    match byte {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

fn decode_utf8(bytes: &[u8], start: usize) -> (u32, usize) {
    let byte = bytes[start];
    let len = utf8_sequence_len(byte);
    if len <= 1 || start + len > bytes.len() {
        let codepoint = if byte < 0x80 {
            u32::from(byte)
        } else {
            REPLACEMENT_CODEPOINT
        };
        return (codepoint, 1);
    }
    std::str::from_utf8(&bytes[start..start + len])
        .ok()
        .and_then(|text| text.chars().next())
        .map_or((REPLACEMENT_CODEPOINT, 1), |character| {
            (u32::from(character), len)
        })
}

fn display_unit(bytes: &[u8], start: usize) -> (usize, u8) {
    let window = &bytes[start..bytes.len().min(start + DISPLAY_UNIT_WINDOW)];
    let valid = match std::str::from_utf8(window) {
        Ok(text) => text,
        Err(error) => std::str::from_utf8(&window[..error.valid_up_to()]).unwrap_or_default(),
    };
    if valid.is_empty() {
        return (1, 1);
    }
    let unit = display_unit_at(valid, 0);
    (
        unit.byte_len.max(1),
        u8::try_from(unit.cell_width).unwrap_or(2),
    )
}

#[cfg(test)]
mod tests;
