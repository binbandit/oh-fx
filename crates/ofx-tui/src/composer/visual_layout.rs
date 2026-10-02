use super::pasted_blocks::{PastedBlock, registered_placeholder_span_starting_at};
use ofx_text::{display_unit_at, next_tab_stop_column, should_wrap_at, visible_width};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerticalDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BreakKind {
    HardNewline,
    SoftWrap,
    InputEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputPrefix {
    pub(crate) text: &'static str,
    pub(crate) cell_width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnitKind {
    Text,
    Tab,
    PastePlaceholder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CursorPoint {
    pub(crate) raw_offset: usize,
    pub(crate) row_index: usize,
    pub(crate) content_column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LayoutUnit {
    pub(crate) raw_start: usize,
    pub(crate) raw_end: usize,
    pub(crate) row_index: usize,
    pub(crate) content_column: usize,
    pub(crate) cell_width: usize,
    pub(crate) kind: UnitKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LayoutRow {
    pub(crate) index: usize,
    pub(crate) raw_start: usize,
    pub(crate) raw_end: usize,
    pub(crate) break_kind: BreakKind,
    pub(crate) content_width: usize,
    pub(crate) last_cursor_offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LayoutSummary {
    pub(crate) total_rows: usize,
    pub(crate) cursor: CursorPoint,
    pub(crate) anchor: Option<CursorPoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutEvent {
    Unit(LayoutUnit),
    RowEnd(LayoutRow),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VerticalScan {
    pub(crate) target: Option<CursorPoint>,
    pub(crate) preferred_column: usize,
    pub(crate) total_rows: usize,
    pub(crate) cursor_row: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct VisualLayout<'a> {
    input: &'a str,
    cursor: usize,
    terminal_cols: u16,
    pasted_blocks: &'a [PastedBlock],
}

#[derive(Debug, Clone, Copy)]
struct RawUnit {
    raw_start: usize,
    raw_end: usize,
    cell_width: usize,
    kind: UnitKind,
}

#[derive(Debug, Clone)]
pub(crate) struct LayoutEvents<'a> {
    layout: VisualLayout<'a>,
    index: usize,
    row_index: usize,
    row_start: usize,
    content_column: usize,
    last_cursor_offset: usize,
    previous_cursor_offset: usize,
    force_next_positive_wrap: bool,
    at_word_start: bool,
    done: bool,
}

impl<'a> VisualLayout<'a> {
    pub(crate) fn new(
        input: &'a str,
        cursor: usize,
        terminal_cols: u16,
        pasted_blocks: &'a [PastedBlock],
    ) -> Self {
        Self {
            input,
            cursor,
            terminal_cols,
            pasted_blocks,
        }
    }

    pub(crate) fn events(&self) -> LayoutEvents<'a> {
        LayoutEvents {
            layout: *self,
            index: 0,
            row_index: 0,
            row_start: 0,
            content_column: 0,
            last_cursor_offset: 0,
            previous_cursor_offset: 0,
            force_next_positive_wrap: false,
            at_word_start: true,
            done: false,
        }
    }

    pub(crate) fn summary(&self, raw_anchor: Option<usize>) -> LayoutSummary {
        LayoutSummary {
            total_rows: self.count_rows(),
            cursor: self.point_at(self.cursor),
            anchor: raw_anchor.map(|anchor| self.point_at(anchor)),
        }
    }

    pub(crate) fn point_at(&self, raw_offset: usize) -> CursorPoint {
        let target = raw_offset.min(self.input.len());
        let mut pending_after = None;
        for event in self.events() {
            match event {
                LayoutEvent::Unit(unit) => {
                    if let Some(point) = point_within_unit(unit, target) {
                        return point;
                    }
                    if target == unit.raw_end {
                        pending_after = Some(point_after_unit(unit));
                    }
                }
                LayoutEvent::RowEnd(row) => {
                    if target == row.raw_end {
                        if row.break_kind != BreakKind::SoftWrap {
                            return point_at_row_end(row);
                        }
                        pending_after = None;
                    } else if let Some(point) = pending_after
                        && pending_resolves_at(point, row)
                    {
                        return point;
                    }
                }
            }
        }
        CursorPoint {
            raw_offset: target,
            row_index: 0,
            content_column: 0,
        }
    }

    pub(crate) fn point_at_cell(
        &self,
        row_index: usize,
        content_column: usize,
    ) -> Option<CursorPoint> {
        let mut target = TargetBuilder::new(content_column);
        for event in self.events() {
            match event {
                LayoutEvent::Unit(unit) => {
                    if unit.row_index == row_index {
                        target.consider(point_at_unit_start(unit));
                    } else if unit.row_index > row_index {
                        return None;
                    }
                }
                LayoutEvent::RowEnd(row) => {
                    if row.index < row_index {
                        continue;
                    }
                    if row.index > row_index {
                        return None;
                    }
                    if row.break_kind != BreakKind::SoftWrap {
                        target.consider(point_at_row_end(row));
                    }
                    return target.result();
                }
            }
        }
        None
    }

    pub(crate) fn scan_adjacent_row(
        &self,
        direction: VerticalDirection,
        preferred_column: Option<usize>,
    ) -> VerticalScan {
        let target_offset = self.cursor.min(self.input.len());
        let mut events = self.events();
        let mut current_row_checkpoint = events.clone();
        let mut previous_row_checkpoint: Option<LayoutEvents<'a>> = None;
        let mut total_rows = 0;
        let mut pending_after = None;
        let mut previous_row_target = None;
        let mut scan = ScanState::new(direction, preferred_column);
        let mut row_builder = TargetBuilder::new(scan.preferred);

        while let Some(event) = events.next() {
            match event {
                LayoutEvent::Unit(unit) => {
                    if preferred_column.is_some() {
                        row_builder.consider(point_at_unit_start(unit));
                    }
                    scan.consider_target_unit(unit);
                    if !scan.cursor_found {
                        if let Some(point) = point_within_unit(unit, target_offset) {
                            scan.found_cursor(
                                point,
                                previous_row_checkpoint.as_ref(),
                                previous_row_target,
                            );
                        } else if target_offset == unit.raw_end {
                            pending_after = Some(point_after_unit(unit));
                        }
                    }
                }
                LayoutEvent::RowEnd(row) => {
                    total_rows += 1;
                    if preferred_column.is_some() && row.break_kind != BreakKind::SoftWrap {
                        row_builder.consider(point_at_row_end(row));
                    }
                    scan.finish_target_row(row);
                    if !scan.cursor_found {
                        if target_offset == row.raw_end {
                            if row.break_kind == BreakKind::SoftWrap {
                                pending_after = None;
                            } else {
                                scan.found_cursor(
                                    point_at_row_end(row),
                                    previous_row_checkpoint.as_ref(),
                                    previous_row_target,
                                );
                            }
                        } else if let Some(point) = pending_after
                            && pending_resolves_at(point, row)
                        {
                            scan.found_cursor(
                                point,
                                previous_row_checkpoint.as_ref(),
                                previous_row_target,
                            );
                        }
                    }
                    if preferred_column.is_some() {
                        previous_row_target = row_builder.result();
                        row_builder = TargetBuilder::new(scan.preferred);
                    }
                    previous_row_checkpoint = Some(current_row_checkpoint);
                    current_row_checkpoint = events.clone();
                }
            }
        }

        VerticalScan {
            target: scan.target,
            preferred_column: scan.preferred,
            total_rows,
            cursor_row: scan.cursor_row,
        }
    }

    pub(crate) fn scan_row_delta(
        &self,
        direction: VerticalDirection,
        row_count: usize,
        preferred_column: Option<usize>,
    ) -> VerticalScan {
        let summary = self.summary(None);
        let preferred = preferred_column.unwrap_or(summary.cursor.content_column);
        let target_row = match direction {
            VerticalDirection::Up => summary.cursor.row_index.saturating_sub(row_count),
            VerticalDirection::Down => summary
                .cursor
                .row_index
                .saturating_add(row_count)
                .min(summary.total_rows.saturating_sub(1)),
        };
        VerticalScan {
            target: if target_row == summary.cursor.row_index {
                None
            } else {
                self.point_at_cell(target_row, preferred)
            },
            preferred_column: preferred,
            total_rows: summary.total_rows,
            cursor_row: summary.cursor.row_index,
        }
    }

    fn count_rows(&self) -> usize {
        self.events()
            .filter(|event| matches!(event, LayoutEvent::RowEnd(_)))
            .count()
    }
}

impl Iterator for LayoutEvents<'_> {
    type Item = LayoutEvent;

    fn next(&mut self) -> Option<LayoutEvent> {
        if self.done {
            return None;
        }
        let input = self.layout.input;
        if self.index >= input.len() {
            return Some(self.finish_row(BreakKind::InputEnd, input.len()));
        }
        if input.as_bytes()[self.index] == b'\n' {
            return Some(self.finish_row(BreakKind::HardNewline, self.index));
        }
        let raw = self.peek_raw_unit();
        if self.should_break_before(raw) {
            return Some(self.finish_row(BreakKind::SoftWrap, raw.raw_start));
        }
        Some(self.consume_raw_unit(raw))
    }
}

impl LayoutEvents<'_> {
    fn should_break_before(&self, raw: RawUnit) -> bool {
        if raw.kind == UnitKind::Text && self.layout.input.as_bytes()[raw.raw_start] == b' ' {
            return false;
        }
        self.should_soft_wrap_word(raw)
            || (raw.cell_width > 0 && self.should_soft_wrap(raw.cell_width))
    }

    fn finish_row(&mut self, break_kind: BreakKind, raw_end: usize) -> LayoutEvent {
        let last_cursor_offset = match break_kind {
            BreakKind::SoftWrap if self.last_cursor_offset == raw_end => {
                self.previous_cursor_offset
            }
            BreakKind::SoftWrap => self.last_cursor_offset,
            BreakKind::HardNewline | BreakKind::InputEnd => raw_end,
        };
        let row = LayoutRow {
            index: self.row_index,
            raw_start: self.row_start,
            raw_end,
            break_kind,
            content_width: self.content_column,
            last_cursor_offset,
        };
        match break_kind {
            BreakKind::InputEnd => self.done = true,
            BreakKind::HardNewline => self.start_next_row(raw_end + 1),
            BreakKind::SoftWrap => self.start_next_row(raw_end),
        }
        LayoutEvent::RowEnd(row)
    }

    fn start_next_row(&mut self, raw_start: usize) {
        self.index = raw_start;
        self.row_index += 1;
        self.row_start = raw_start;
        self.content_column = 0;
        self.last_cursor_offset = raw_start;
        self.previous_cursor_offset = raw_start;
        self.force_next_positive_wrap = false;
        self.at_word_start = true;
    }

    fn peek_raw_unit(&self) -> RawUnit {
        let input = self.layout.input;
        let start = self.index;
        if let Some(span) =
            registered_placeholder_span_starting_at(input, start, self.layout.pasted_blocks)
        {
            return RawUnit {
                raw_start: span.raw_start,
                raw_end: span.raw_end,
                cell_width: visible_width(&input[span.raw_start..span.raw_end]),
                kind: UnitKind::PastePlaceholder,
            };
        }
        if input.as_bytes()[start] == b'\t' {
            return RawUnit {
                raw_start: start,
                raw_end: start + 1,
                cell_width: tab_advance(
                    input_prefix(self.row_index).cell_width,
                    self.content_column,
                    self.layout.terminal_cols,
                ),
                kind: UnitKind::Tab,
            };
        }
        let unit = display_unit_at(input, start);
        RawUnit {
            raw_start: start,
            raw_end: start + unit.byte_len.max(1),
            cell_width: unit.cell_width,
            kind: UnitKind::Text,
        }
    }

    fn consume_raw_unit(&mut self, raw: RawUnit) -> LayoutEvent {
        let unit = LayoutUnit {
            raw_start: raw.raw_start,
            raw_end: raw.raw_end,
            row_index: self.row_index,
            content_column: self.content_column,
            cell_width: raw.cell_width,
            kind: raw.kind,
        };
        self.index = raw.raw_end;
        self.previous_cursor_offset = self.last_cursor_offset;
        self.last_cursor_offset = raw.raw_end;
        self.at_word_start = match raw.kind {
            UnitKind::Text => is_word_break_byte(self.layout.input.as_bytes()[raw.raw_start]),
            UnitKind::Tab | UnitKind::PastePlaceholder => true,
        };
        let available = self.available_content_cells();
        let oversized_empty_row =
            available > 0 && self.content_column == 0 && raw.cell_width > available;
        self.content_column = self.content_column.saturating_add(raw.cell_width);
        if oversized_empty_row {
            self.force_next_positive_wrap = true;
        }
        LayoutEvent::Unit(unit)
    }

    fn should_soft_wrap_word(&self, raw: RawUnit) -> bool {
        if raw.kind != UnitKind::Text
            || !self.at_word_start
            || self.content_column == 0
            || self.force_next_positive_wrap
        {
            return false;
        }
        let available = self.available_content_cells();
        if self.content_column >= available {
            return false;
        }
        let remaining = available - self.content_column;
        let next_available = next_row_available_cells(self.row_index, self.layout.terminal_cols);
        if next_available == 0 {
            return false;
        }
        let word_width = self.measure_word_width(raw.raw_start, next_available);
        word_width > remaining && word_width <= next_available
    }

    fn measure_word_width(&self, start: usize, cap: usize) -> usize {
        let input = self.layout.input;
        let mut width = 0_usize;
        let mut index = start;
        while index < input.len() && !self.word_ends_at(index) {
            let unit = display_unit_at(input, index);
            width = width.saturating_add(unit.cell_width);
            if width > cap {
                break;
            }
            index += unit.byte_len.max(1);
        }
        width
    }

    fn word_ends_at(&self, index: usize) -> bool {
        is_word_break_byte(self.layout.input.as_bytes()[index])
            || registered_placeholder_span_starting_at(
                self.layout.input,
                index,
                self.layout.pasted_blocks,
            )
            .is_some()
    }

    fn should_soft_wrap(&self, cell_width: usize) -> bool {
        let available = self.available_content_cells();
        if available == 0 {
            return false;
        }
        if self.force_next_positive_wrap {
            return true;
        }
        if self.content_column == 0 {
            return false;
        }
        if self.content_column >= available {
            return true;
        }
        let Ok(width) = u16::try_from(cell_width) else {
            return true;
        };
        let start_column = input_prefix(self.row_index).cell_width + self.content_column + 1;
        should_wrap_at(
            u16::try_from(start_column).unwrap_or(u16::MAX),
            width,
            self.layout.terminal_cols,
        )
    }

    fn available_content_cells(&self) -> usize {
        usize::from(self.layout.terminal_cols)
            .saturating_sub(input_prefix(self.row_index).cell_width)
    }
}

#[derive(Debug, Clone, Copy)]
struct TargetBuilder {
    preferred_column: usize,
    first: Option<CursorPoint>,
    best: Option<CursorPoint>,
}

impl TargetBuilder {
    fn new(preferred_column: usize) -> Self {
        Self {
            preferred_column,
            first: None,
            best: None,
        }
    }

    fn consider(&mut self, point: CursorPoint) {
        if self.first.is_none() {
            self.first = Some(point);
        }
        if point.content_column > self.preferred_column {
            return;
        }
        let better = self.best.is_none_or(|current| {
            point.content_column > current.content_column
                || (point.content_column == current.content_column
                    && point.raw_offset > current.raw_offset)
        });
        if better {
            self.best = Some(point);
        }
    }

    fn result(self) -> Option<CursorPoint> {
        self.best.or(self.first)
    }
}

struct ScanState {
    direction: VerticalDirection,
    preferred_column: Option<usize>,
    preferred: usize,
    cursor_found: bool,
    cursor_row: usize,
    target: Option<CursorPoint>,
    target_row: Option<usize>,
    target_builder: TargetBuilder,
}

impl ScanState {
    fn new(direction: VerticalDirection, preferred_column: Option<usize>) -> Self {
        Self {
            direction,
            preferred_column,
            preferred: preferred_column.unwrap_or(0),
            cursor_found: false,
            cursor_row: 0,
            target: None,
            target_row: None,
            target_builder: TargetBuilder::new(0),
        }
    }

    fn found_cursor(
        &mut self,
        point: CursorPoint,
        previous_row_checkpoint: Option<&LayoutEvents<'_>>,
        previous_row_target: Option<CursorPoint>,
    ) {
        self.cursor_found = true;
        self.cursor_row = point.row_index;
        if self.preferred_column.is_none() {
            self.preferred = point.content_column;
        }
        match self.direction {
            VerticalDirection::Up => {
                if point.row_index > 0 {
                    self.target = if self.preferred_column.is_none() {
                        previous_row_checkpoint.and_then(|checkpoint| {
                            target_point_in_checkpoint(
                                checkpoint.clone(),
                                point.row_index - 1,
                                self.preferred,
                            )
                        })
                    } else {
                        previous_row_target
                    };
                }
            }
            VerticalDirection::Down => {
                self.target_row = Some(point.row_index + 1);
                self.target_builder = TargetBuilder::new(self.preferred);
            }
        }
    }

    fn consider_target_unit(&mut self, unit: LayoutUnit) {
        if self.target_row == Some(unit.row_index) {
            self.target_builder.consider(point_at_unit_start(unit));
        }
    }

    fn finish_target_row(&mut self, row: LayoutRow) {
        if self.target_row != Some(row.index) {
            return;
        }
        if row.break_kind != BreakKind::SoftWrap {
            self.target_builder.consider(point_at_row_end(row));
        }
        self.target = self.target_builder.result();
        self.target_row = None;
    }
}

fn target_point_in_checkpoint(
    checkpoint: LayoutEvents<'_>,
    row_index: usize,
    preferred_column: usize,
) -> Option<CursorPoint> {
    let mut builder = TargetBuilder::new(preferred_column);
    for event in checkpoint {
        match event {
            LayoutEvent::Unit(unit) if unit.row_index == row_index => {
                builder.consider(point_at_unit_start(unit));
            }
            LayoutEvent::RowEnd(row) if row.index == row_index => {
                if row.break_kind != BreakKind::SoftWrap {
                    builder.consider(point_at_row_end(row));
                }
                return builder.result();
            }
            LayoutEvent::Unit(_) | LayoutEvent::RowEnd(_) => {}
        }
    }
    builder.result()
}

fn point_at_unit_start(unit: LayoutUnit) -> CursorPoint {
    CursorPoint {
        raw_offset: unit.raw_start,
        row_index: unit.row_index,
        content_column: unit.content_column,
    }
}

fn point_after_unit(unit: LayoutUnit) -> CursorPoint {
    CursorPoint {
        raw_offset: unit.raw_end,
        row_index: unit.row_index,
        content_column: unit.content_column.saturating_add(unit.cell_width),
    }
}

fn point_at_row_end(row: LayoutRow) -> CursorPoint {
    CursorPoint {
        raw_offset: row.raw_end,
        row_index: row.index,
        content_column: row.content_width,
    }
}

fn point_within_unit(unit: LayoutUnit, target: usize) -> Option<CursorPoint> {
    (target >= unit.raw_start && target < unit.raw_end).then(|| point_at_unit_start(unit))
}

fn pending_resolves_at(point: CursorPoint, row: LayoutRow) -> bool {
    point.raw_offset < row.raw_end || row.break_kind != BreakKind::SoftWrap
}

fn is_word_break_byte(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n')
}

fn next_row_available_cells(current_row_index: usize, terminal_cols: u16) -> usize {
    usize::from(terminal_cols).saturating_sub(input_prefix(current_row_index + 1).cell_width)
}

pub(crate) fn input_prefix(row_index: usize) -> InputPrefix {
    if row_index == 0 {
        InputPrefix {
            text: "❯ ",
            cell_width: 2,
        }
    } else {
        InputPrefix {
            text: "  ",
            cell_width: 2,
        }
    }
}

fn tab_advance(prefix_cell_width: usize, content_column: usize, terminal_cols: u16) -> usize {
    if terminal_cols == 0 {
        return 0;
    }
    let right_margin = usize::from(terminal_cols) - 1;
    let current_zero_based = prefix_cell_width
        .saturating_add(content_column)
        .min(right_margin);
    let current_col = u16::try_from(current_zero_based + 1).unwrap_or(terminal_cols);
    usize::from(next_tab_stop_column(current_col, terminal_cols) - current_col)
}

#[cfg(test)]
mod tests {
    use super::super::entity_spans::Span;
    use super::*;

    fn layout(input: &str, cursor: usize, cols: u16) -> VisualLayout<'_> {
        VisualLayout::new(input, cursor, cols, &[])
    }

    fn point(raw_offset: usize, row_index: usize, content_column: usize) -> CursorPoint {
        CursorPoint {
            raw_offset,
            row_index,
            content_column,
        }
    }

    fn expect_cursor(
        input: &str,
        raw_offset: usize,
        cols: u16,
        row_index: usize,
        content_column: usize,
    ) {
        assert_eq!(
            layout(input, raw_offset, cols).point_at(raw_offset),
            point(raw_offset, row_index, content_column),
            "{input:?} at {raw_offset}"
        );
    }

    fn row_at(layout: VisualLayout<'_>, index: usize) -> Option<LayoutRow> {
        layout.events().find_map(|event| match event {
            LayoutEvent::RowEnd(row) if row.index == index => Some(row),
            _ => None,
        })
    }

    fn row_text(input: &str, row: LayoutRow) -> &str {
        &input[row.raw_start..row.raw_end]
    }

    #[test]
    fn visual_layout_preserves_empty_input_hard_newlines_and_trailing_empty_rows() {
        let empty = layout("", 0, 80).summary(None);
        assert_eq!(empty.total_rows, 1);
        assert_eq!(empty.cursor, point(0, 0, 0));
        assert_eq!(
            row_at(layout("", 0, 80), 0).unwrap().break_kind,
            BreakKind::InputEnd
        );

        assert_eq!(layout("a\nb", 0, 80).summary(None).total_rows, 2);
        expect_cursor("a\nb", 1, 80, 0, 1);
        expect_cursor("a\nb", 2, 80, 1, 0);

        let multi = layout("a\n\nb\n", 5, 80).summary(None);
        assert_eq!(multi.total_rows, 4);
        assert_eq!(multi.cursor, point(5, 3, 0));
    }

    #[test]
    fn visual_layout_targets_registered_paste_placeholders_only_at_boundaries() {
        let input = "x\n[Pasted text #7, 1 line]";
        let blocks = [PastedBlock {
            id: 7,
            text: "P".repeat(1001),
            line_count: 1,
            span: Span::new("x\n".len(), input.len()),
        }];
        let scan = VisualLayout::new(input, 1, 80, &blocks)
            .scan_adjacent_row(VerticalDirection::Down, None);
        assert_eq!(scan.target.unwrap().raw_offset, "x\n".len());
    }

    #[test]
    fn visual_layout_assigns_soft_wrap_boundaries_to_the_following_row() {
        let source = layout("abcd", 3, 5);
        let summary = source.summary(None);
        assert_eq!(summary.total_rows, 2);
        assert_eq!(summary.cursor, point(3, 1, 0));

        let first = row_at(source, 0).unwrap();
        assert_eq!(first.break_kind, BreakKind::SoftWrap);
        assert_eq!(first.raw_start, 0);
        assert_eq!(first.raw_end, 3);
        assert!(first.last_cursor_offset < first.raw_end);
    }

    #[test]
    fn visual_layout_zero_capacity_widths_split_only_on_hard_newlines() {
        for cols in [0, 1, 2] {
            let source = layout("abc\ndef", "abc\ndef".len(), cols);
            let summary = source.summary(None);
            assert_eq!(summary.total_rows, 2);
            assert_eq!(summary.cursor, point("abc\ndef".len(), 1, 3));
            assert_eq!(
                row_at(source, 0).unwrap().break_kind,
                BreakKind::HardNewline
            );
        }
    }

    #[test]
    fn visual_layout_handles_wide_runes_oversized_rows_combining_marks_and_invalid_utf8() {
        expect_cursor("a界", 1, 4, 1, 0);
        assert_eq!(layout("a界", 2, 4).point_at(2), point(1, 1, 0));

        assert_eq!(layout("界a", 0, 3).summary(None).total_rows, 2);
        for cols in [0, 1, 2] {
            assert_eq!(layout("界a", 0, cols).summary(None).total_rows, 1);
        }

        let combining = "a\u{301}\nb";
        expect_cursor(combining, "a\u{301}".len(), 80, 0, 1);
        let down = layout(combining, 0, 80).scan_adjacent_row(VerticalDirection::Down, None);
        assert_eq!(down.total_rows, 2);
        assert_eq!(down.cursor_row, 0);
        assert_eq!(down.target, Some(point("a\u{301}\n".len(), 1, 0)));

        let combining_target =
            layout("x\na\u{301}\nz", 1, 80).scan_adjacent_row(VerticalDirection::Down, None);
        assert_eq!(combining_target.total_rows, 3);
        assert_eq!(
            combining_target.target,
            Some(point("x\na\u{301}".len(), 1, 1))
        );
    }

    #[test]
    fn visual_layout_keeps_unicode_display_units_atomic_at_the_right_margin() {
        let emoji = "\u{1F44D}\u{1F3FD}";
        let input = format!("a{emoji}b");
        let source = layout(&input, 0, 5);
        assert_eq!(source.summary(None).total_rows, 2);
        assert_eq!(source.point_at(1), point(1, 0, 1));
        assert_eq!(source.point_at(2), point(1, 0, 1));
        assert_eq!(
            source.point_at(1 + emoji.len()),
            point(1 + emoji.len(), 1, 0)
        );
        assert_eq!(source.point_at(input.len()), point(input.len(), 1, 1));

        let unit = source
            .events()
            .find_map(|event| match event {
                LayoutEvent::Unit(unit) if unit.raw_start == 1 => Some(unit),
                _ => None,
            })
            .unwrap();
        assert_eq!(unit.raw_end, 1 + emoji.len());
        assert_eq!(unit.cell_width, 2);
        assert_eq!(unit.kind, UnitKind::Text);
    }

    #[test]
    fn visual_layout_tab_units_use_absolute_stops_and_strict_following_overflow() {
        assert_eq!(tab_advance(2, 0, 10), 6);
        assert_eq!(tab_advance(2, 6, 10), 1);
        assert_eq!(tab_advance(2, 7, 10), 0);
        assert_eq!(tab_advance(2, 0, 0), 0);

        assert_eq!(layout("\tX", 0, 10).summary(None).total_rows, 1);
        assert_eq!(layout("aaaaaa\t界", 0, 10).summary(None).total_rows, 2);
        assert_eq!(layout("aaaaaa\tXY", 0, 10).summary(None).total_rows, 2);
        expect_cursor("\tX", 1, 10, 0, 6);
    }

    #[test]
    fn visual_layout_exposes_zero_cell_tab_targets_without_forcing_soft_wraps() {
        let at_margin = layout("\tX", 0, 3);
        assert_eq!(at_margin.summary(None).total_rows, 1);
        assert_eq!(at_margin.point_at(0), point(0, 0, 0));
        assert_eq!(at_margin.point_at(1), point(1, 0, 0));
        assert_eq!(at_margin.point_at(2), point(2, 0, 1));

        for cols in [0, 1, 2] {
            let source = layout("\tX", 2, cols);
            assert_eq!(source.summary(None).total_rows, 1);
            assert_eq!(source.point_at(1), point(1, 0, 0));
            assert_eq!(source.point_at(2), point(2, 0, 1));
        }
    }

    #[test]
    fn visual_layout_clamps_cursor_beyond_input_and_short_target_rows() {
        assert_eq!(layout("abc", 99, 80).point_at(99), point(3, 0, 3));
        let short = layout("abcdef\nx", "abcdef".len(), 80)
            .scan_adjacent_row(VerticalDirection::Down, None);
        assert_eq!(short.preferred_column, 6);
        assert_eq!(short.target, Some(point("abcdef\nx".len(), 1, 1)));
    }

    #[test]
    fn visual_layout_maps_pointer_cells_to_legal_cursor_boundaries() {
        let ascii = layout("abc", 0, 80);
        assert_eq!(ascii.point_at_cell(0, 0), Some(point(0, 0, 0)));
        assert_eq!(ascii.point_at_cell(0, 1), Some(point(1, 0, 1)));
        assert_eq!(ascii.point_at_cell(0, 99), Some(point(3, 0, 3)));
        assert_eq!(ascii.point_at_cell(1, 0), None);

        let hard = layout("ab\nc", 0, 80);
        assert_eq!(hard.point_at_cell(0, 99), Some(point(2, 0, 2)));
        assert_eq!(hard.point_at_cell(1, 0), Some(point(3, 1, 0)));

        let wrapped = layout("abcdef", 0, 6);
        assert_eq!(wrapped.point_at_cell(0, 99), Some(point(3, 0, 3)));
        assert_eq!(wrapped.point_at_cell(1, 0), Some(point(4, 1, 0)));

        let wide = layout("a界b", 0, 80);
        assert_eq!(wide.point_at_cell(0, 1), Some(point(1, 0, 1)));
        assert_eq!(wide.point_at_cell(0, 2), Some(point(1, 0, 1)));
        assert_eq!(wide.point_at_cell(0, 3), Some(point(4, 0, 3)));
    }

    #[test]
    fn visual_layout_pointer_mapping_keeps_paste_placeholders_atomic() {
        let placeholder = "[Pasted text #1, 1 line]";
        let input = format!("{placeholder} x");
        let blocks = [PastedBlock {
            id: 1,
            text: "pasted".to_owned(),
            line_count: 1,
            span: Span::new(0, placeholder.len()),
        }];
        let source = VisualLayout::new(&input, 0, 80, &blocks);
        assert_eq!(source.point_at_cell(0, 3), Some(point(0, 0, 0)));
        assert_eq!(
            source.point_at_cell(0, placeholder.len()),
            Some(point(placeholder.len(), 0, placeholder.len()))
        );
    }

    #[test]
    fn visual_layout_soft_wrap_target_end_stays_owned_by_the_source_row() {
        let scan = layout("abcd", 4, 5).scan_adjacent_row(VerticalDirection::Up, Some(99));
        assert_eq!(scan.total_rows, 2);
        assert_eq!(scan.cursor_row, 1);
        assert_eq!(scan.target, Some(point(2, 0, 2)));
    }

    #[test]
    fn visual_layout_first_vertical_move_captures_source_column_with_target() {
        let up =
            layout("\nabcdef", "\nabcdef".len(), 80).scan_adjacent_row(VerticalDirection::Up, None);
        assert_eq!(up.preferred_column, 6);
        assert_eq!(up.target, Some(point(0, 0, 0)));

        let down =
            layout("abcdef\n", "abcdef".len(), 80).scan_adjacent_row(VerticalDirection::Down, None);
        assert_eq!(down.preferred_column, 6);
        assert_eq!(down.target, Some(point("abcdef\n".len(), 1, 0)));
    }

    #[test]
    fn visual_layout_target_selection_and_vertical_scan_report_complete_row_facts() {
        let hard =
            layout("abc\nde", "abc\nde".len(), 80).scan_adjacent_row(VerticalDirection::Up, None);
        assert_eq!(hard.total_rows, 2);
        assert_eq!(hard.cursor_row, 1);
        assert_eq!(hard.target, Some(point(2, 0, 2)));

        let soft = layout("abcdefghij", 1, 5).scan_adjacent_row(VerticalDirection::Down, None);
        assert!(soft.target.is_some());
        assert_eq!(soft.total_rows, 4);
        assert_eq!(soft.cursor_row, 0);
        assert_eq!(
            soft.total_rows,
            layout("abcdefghij", 1, 5).summary(None).total_rows
        );

        let none = layout("abc", 0, 80).scan_adjacent_row(VerticalDirection::Up, None);
        assert_eq!(none.target, None);
        assert_eq!(none.total_rows, 1);
        assert_eq!(none.cursor_row, 0);
    }

    #[test]
    fn visual_layout_row_delta_reuses_the_measured_row_coordinate_model() {
        let input = "one\ntwo\nthree\nfour";
        let down = layout(input, 1, 80).scan_row_delta(VerticalDirection::Down, 2, None);
        assert_eq!(down.total_rows, 4);
        assert_eq!(down.cursor_row, 0);
        assert_eq!(down.target.unwrap().raw_offset, "one\ntwo\n".len() + 1);
        assert_eq!(down.preferred_column, 1);

        let clamped =
            layout(input, input.len(), 80).scan_row_delta(VerticalDirection::Down, 10, None);
        assert_eq!(clamped.target, None);
    }

    #[test]
    fn visual_layout_wraps_whole_words_at_the_soft_margin() {
        let input = "hello brave world";
        let source = layout(input, input.len(), 12);
        assert_eq!(source.summary(None).total_rows, 3);
        let first = row_at(source, 0).unwrap();
        assert_eq!(first.break_kind, BreakKind::SoftWrap);
        assert_eq!(row_text(input, first), "hello ");
        assert_eq!(row_text(input, row_at(source, 1).unwrap()), "brave ");
        assert_eq!(row_text(input, row_at(source, 2).unwrap()), "world");
        assert_eq!(source.point_at("hello ".len()), point("hello ".len(), 1, 0));
    }

    #[test]
    fn visual_layout_still_splits_words_wider_than_a_full_row_per_character() {
        let input = "ab cdefghijklm";
        let source = layout(input, input.len(), 12);
        assert_eq!(source.summary(None).total_rows, 2);
        let first = row_at(source, 0).unwrap();
        assert_eq!(first.break_kind, BreakKind::SoftWrap);
        assert_eq!(first.content_width, 10);
    }

    #[test]
    fn visual_layout_hangs_margin_spaces_so_continuation_rows_never_start_with_a_space() {
        let input = "abcdefghij kl";
        let source = layout(input, input.len(), 12);
        assert_eq!(source.summary(None).total_rows, 2);
        let first = row_at(source, 0).unwrap();
        assert_eq!(first.break_kind, BreakKind::SoftWrap);
        assert_eq!(row_text(input, first), "abcdefghij ");
        assert_eq!(row_text(input, row_at(source, 1).unwrap()), "kl");

        let multi = "abcdefghij   kl";
        let multi_source = layout(multi, multi.len(), 12);
        assert_eq!(multi_source.summary(None).total_rows, 2);
        assert_eq!(row_text(multi, row_at(multi_source, 1).unwrap()), "kl");
    }

    #[test]
    fn visual_layout_word_wrap_keeps_trailing_space_on_the_previous_row() {
        let input = "aaaa bbbbbb cc";
        let source = layout(input, input.len(), 12);
        assert_eq!(source.summary(None).total_rows, 2);
        assert_eq!(row_text(input, row_at(source, 0).unwrap()), "aaaa ");
        assert_eq!(row_text(input, row_at(source, 1).unwrap()), "bbbbbb cc");
    }

    #[test]
    fn visual_layout_handles_direct_limit_and_longer_restored_input_without_allocation() {
        let direct = "x".repeat(4096);
        let longer = "y".repeat(5000);
        let direct_summary = layout(&direct, direct.len(), 80).summary(None);
        let longer_summary = layout(&longer, longer.len(), 80).summary(None);
        assert!(direct_summary.total_rows > 1);
        assert!(longer_summary.total_rows > direct_summary.total_rows);
        assert_eq!(
            layout(&direct, direct.len(), 80).count_rows(),
            direct_summary.total_rows
        );
    }

    #[test]
    fn visual_layout_summary_projects_a_selection_anchor() {
        let source = layout("one\ntwo", 6, 80);
        let summary = source.summary(Some(1));
        assert_eq!(summary.cursor, point(6, 1, 2));
        assert_eq!(summary.anchor, Some(point(1, 0, 1)));
    }
}
