use crate::presentation::ansi::{
    BULLET_MARKER, MAX_TABLE_CELLS_PER_SOURCE_BYTE, TABLE_COLUMN_SEP, TABLE_HORIZ, TABLE_JUNCTION,
    TASK_COMPLETED_MARKER, TASK_PENDING_MARKER, VERTICAL_RULE_PREFIX, write_dim,
};
use crate::presentation::block_parse::{
    BlockquotePrefix, ParsedTaskListItem, parse_header, parse_ordered_list, parse_task_list_item,
    parse_unordered_list, table_lines,
};
use crate::presentation::inline_render::{write_inline, write_inline_no_bold};
use crate::presentation::payload::{
    CodeBlockPayload, FootnoteSink, TableColumnAlign, TablePayload, TableRow,
};
use crate::presentation::text_util::{
    EscapedPunctuation, left_trim, nth_line, without_terminal_hard_break_marker,
};
use crate::styled::{Attr, Hang, Slot, Span, SpanWriter, TextOut, spans_width};

pub(crate) fn write_heading(
    level: usize,
    content: &str,
    out: &mut SpanWriter,
    footnotes: Option<&mut FootnoteSink>,
) {
    match level {
        1 => {
            out.open(Attr::Bold);
            out.open(Attr::Underline);
        }
        2 => out.open(Attr::Bold),
        3 => out.open(Attr::Underline),
        4 => {
            out.open(Attr::Bold);
            out.open(Attr::Dim);
        }
        5 => {
            out.open(Attr::Dim);
            out.open(Attr::Underline);
        }
        _ => out.open(Attr::Dim),
    }
    write_inline_no_bold(content, out, matches!(level, 1 | 3 | 5), footnotes);
    match level {
        1 => {
            out.close(Attr::Underline);
            out.close(Attr::Bold);
        }
        3 => out.close(Attr::Underline),
        5 => {
            out.close(Attr::Underline);
            out.close(Attr::Dim);
        }
        _ => out.close(Attr::Bold),
    }
}

pub(crate) fn write_blockquote_line(
    blockquote: BlockquotePrefix,
    content: &str,
    line_has_lf: bool,
    out: &mut SpanWriter,
    mut footnotes: Option<&mut FootnoteSink>,
) {
    out.set_hang(Hang::Quote {
        indent: blockquote.indent,
        depth: blockquote.depth,
    });
    out.repeat(" ", blockquote.indent);
    for _ in 0..blockquote.depth {
        write_dim(out, VERTICAL_RULE_PREFIX);
    }
    let without_break = without_terminal_hard_break_marker(content, line_has_lf);
    if let Some(header) = parse_header(without_break) {
        write_heading(header.level, header.content, out, footnotes);
        return;
    }
    if write_list_line(content, line_has_lf, out, footnotes.as_deref_mut()) {
        return;
    }
    write_inline(without_break, out, false, footnotes);
}

pub(crate) fn write_list_line(
    line: &str,
    line_has_lf: bool,
    out: &mut SpanWriter,
    footnotes: Option<&mut FootnoteSink>,
) -> bool {
    if let Some(parsed) = parse_unordered_list(line) {
        out.text(parsed.indent);
        let task = parse_task_list_item(parsed.content);
        let marker_width = task.map_or(BULLET_MARKER.chars().count(), task_marker_width);
        out.set_hang(list_hang(parsed.indent, marker_width));
        let content = if let Some(task) = task {
            write_task_list_marker(out, task);
            task.content
        } else {
            write_dim(out, BULLET_MARKER);
            parsed.content
        };
        write_inline(
            without_terminal_hard_break_marker(content, line_has_lf),
            out,
            false,
            footnotes,
        );
        return true;
    }
    if let Some(parsed) = parse_ordered_list(line) {
        out.text(parsed.indent);
        write_dim(out, parsed.marker);
        out.text(" ");
        let task = parse_task_list_item(parsed.content);
        let marker_width = parsed.marker.len() + 1 + task.map_or(0, task_marker_width);
        out.set_hang(list_hang(parsed.indent, marker_width));
        let content = match task {
            Some(task) => {
                write_task_list_marker(out, task);
                task.content
            }
            None => parsed.content,
        };
        write_inline(
            without_terminal_hard_break_marker(content, line_has_lf),
            out,
            false,
            footnotes,
        );
        return true;
    }
    false
}

fn list_hang(indent: &str, marker_width: usize) -> Hang {
    if indent.bytes().all(|byte| byte == b' ') {
        Hang::Indent(indent.len() + marker_width)
    } else {
        Hang::None
    }
}

fn task_marker_width(task: ParsedTaskListItem<'_>) -> usize {
    1 + usize::from(task.has_separator)
}

pub(crate) fn write_definition_line(
    body: &str,
    line_has_lf: bool,
    out: &mut TextOut<'_>,
    footnotes: Option<&mut FootnoteSink>,
) {
    out.set_hang(Hang::Indent(2));
    write_dim(out, "  ");
    write_inline(
        without_terminal_hard_break_marker(body, line_has_lf),
        out,
        false,
        footnotes,
    );
    out.newline();
}

pub(crate) fn write_task_list_marker(out: &mut SpanWriter, task: ParsedTaskListItem<'_>) {
    if task.completed {
        out.open_slot(Slot::TaskCompleted);
        out.text(TASK_COMPLETED_MARKER);
        out.close_slot();
        if task.has_separator {
            out.text(" ");
        }
        return;
    }
    out.open(Attr::Dim);
    out.text(TASK_PENDING_MARKER);
    if task.has_separator {
        out.text(" ");
    }
    out.close(Attr::Dim);
}

pub(crate) fn render_table(buf: &str, out: &mut TextOut<'_>, footnotes: Option<&mut FootnoteSink>) {
    let table = parse_table_payload_with_footnotes(buf, footnotes);
    write_table(&table, out);
}

pub(crate) fn write_table(table: &TablePayload, out: &mut TextOut<'_>) {
    let widths = column_widths(table);
    if table.rows.is_empty() || widths.is_empty() {
        return;
    }
    for (row_index, row) in table.rows.iter().enumerate() {
        let is_header = row_index == 0;
        write_table_row(&row.cells, &widths, &table.alignments, is_header, out);
        if is_header {
            write_table_separator(&widths, out);
        }
    }
}

fn column_widths(table: &TablePayload) -> Vec<usize> {
    let column_count = table
        .rows
        .iter()
        .map(|row| row.cells.len())
        .fold(table.column_count, usize::max);
    let mut widths = vec![0; column_count];
    for row in &table.rows {
        for (column, cell) in row.cells.iter().enumerate() {
            widths[column] = widths[column].max(spans_width(cell));
        }
    }
    widths
}

fn write_table_row(
    cells: &[Vec<Span>],
    widths: &[usize],
    alignments: &[TableColumnAlign],
    is_header: bool,
    out: &mut TextOut<'_>,
) {
    for (column, &column_width) in widths.iter().enumerate() {
        if column > 0 {
            out.text(TABLE_COLUMN_SEP);
        }
        let Some(cell) = cells.get(column) else {
            out.repeat(" ", column_width);
            continue;
        };
        let alignment = if is_header {
            TableColumnAlign::Left
        } else {
            alignments.get(column).copied().unwrap_or_default()
        };
        let pad = column_width.saturating_sub(spans_width(cell));
        let left_pad = match alignment {
            TableColumnAlign::Left => 0,
            TableColumnAlign::Right => pad,
            TableColumnAlign::Center => pad / 2,
        };
        out.repeat(" ", left_pad);
        if is_header {
            out.append_spans(&table_header_cell(cell));
        } else {
            out.append_spans(cell);
        }
        out.repeat(" ", pad - left_pad);
    }
    out.newline();
}

pub fn table_header_cell(cell: &[Span]) -> Vec<Span> {
    cell.iter()
        .map(|span| Span {
            style: span.style.with(Attr::Bold),
            ..span.clone()
        })
        .collect()
}

fn write_table_separator(widths: &[usize], out: &mut TextOut<'_>) {
    for (column, &column_width) in widths.iter().enumerate() {
        if column > 0 {
            out.text(TABLE_JUNCTION);
        }
        out.repeat(TABLE_HORIZ, column_width);
    }
    out.newline();
}

fn split_cells(line: &str) -> Vec<&str> {
    let mut rest = left_trim(line);
    rest = rest.strip_prefix('|').unwrap_or(rest);
    rest = rest.trim_end_matches([' ', '\t']);
    if rest.ends_with('|') && !EscapedPunctuation::default().at(rest.as_bytes(), rest.len() - 1) {
        rest = &rest[..rest.len() - 1];
    }

    let bytes = rest.as_bytes();
    let mut escapes = EscapedPunctuation::default();
    let mut cells = Vec::new();
    let mut cell_start = 0;
    for index in 0..=bytes.len() {
        if index == bytes.len() || (bytes[index] == b'|' && !escapes.at(bytes, index)) {
            cells.push(rest[cell_start..index].trim_matches([' ', '\t']));
            cell_start = index + 1;
        }
    }
    cells
}

fn parse_column_alignments(buf: &str, column_count: usize) -> Vec<TableColumnAlign> {
    let mut alignments = vec![TableColumnAlign::Left; column_count];
    let Some(separator_line) = nth_line(buf, 1) else {
        return alignments;
    };
    for (alignment, cell) in alignments.iter_mut().zip(split_cells(separator_line)) {
        let trimmed = cell.trim_matches([' ', '\t']);
        if trimmed.is_empty() {
            continue;
        }
        let starts = trimmed.starts_with(':');
        let ends = trimmed.ends_with(':');
        *alignment = if starts && ends {
            TableColumnAlign::Center
        } else if ends {
            TableColumnAlign::Right
        } else {
            TableColumnAlign::Left
        };
    }
    alignments
}

pub(crate) fn render_table_payload(table: &TablePayload, out: &mut TextOut<'_>) {
    write_table(table, out);
}

pub(crate) fn table_output_stays_proportional(buf: &str) -> bool {
    let mut widths: Vec<usize> = Vec::new();
    let mut rendered_rows: usize = 1;
    for (line_index, line) in table_lines(buf).enumerate() {
        if line_index == 1 {
            continue;
        }
        let cells = split_cells(line);
        if line_index == 0 {
            widths = vec![0; cells.len()];
        }
        for (width, cell) in widths.iter_mut().zip(cells) {
            *width = (*width).max(cell.len());
        }
        rendered_rows += 1;
    }
    let separators = widths.len().saturating_sub(1) * TABLE_COLUMN_SEP.chars().count();
    let row_width = widths.iter().sum::<usize>() + separators;
    rendered_rows.saturating_mul(row_width)
        <= MAX_TABLE_CELLS_PER_SOURCE_BYTE.saturating_mul(buf.len())
}

pub(crate) fn parse_table_payload_with_footnotes(
    buf: &str,
    mut footnotes: Option<&mut FootnoteSink>,
) -> TablePayload {
    let mut rows = Vec::new();
    let mut column_count = 0;
    for (line_index, line) in table_lines(buf).enumerate() {
        if line_index == 1 {
            continue;
        }
        let raw_cells = split_cells(line);
        if line_index == 0 {
            column_count = raw_cells.len();
        }
        let cells = raw_cells
            .into_iter()
            .take(column_count)
            .map(|raw_cell| {
                let mut writer = SpanWriter::default();
                write_inline(raw_cell, &mut writer, false, footnotes.as_deref_mut());
                writer.take_spans()
            })
            .collect();
        rows.push(TableRow { cells });
    }
    TablePayload {
        alignments: parse_column_alignments(buf, column_count),
        rows,
        column_count,
    }
}

pub(crate) fn write_footnote_definition_marker(out: &mut SpanWriter, number: usize) {
    let marker = format!("[{number}] ");
    out.set_hang(Hang::Indent(marker.len()));
    write_dim(out, &marker);
}

pub(crate) fn write_footnote_body(
    body: &str,
    out: &mut TextOut<'_>,
    sink: &mut FootnoteSink,
    number: usize,
) {
    let continuation = " ".repeat(format!("[{number}] ").len());
    for (index, line) in body.split('\n').enumerate() {
        if index > 0 {
            out.newline();
            out.set_hang(Hang::Indent(continuation.len()));
            write_dim(out, &continuation);
        }
        write_inline(line, out, false, Some(&mut *sink));
    }
    out.newline();
}

pub(crate) fn render_code_block_payload(block: &CodeBlockPayload, out: &mut TextOut<'_>) {
    for line in block.code.split_terminator('\n') {
        write_code_line(line, out);
        out.newline();
    }
}

pub(crate) fn write_code_line(line: &str, out: &mut SpanWriter) {
    out.set_hang(Hang::Quote {
        indent: 0,
        depth: 1,
    });
    write_dim(out, VERTICAL_RULE_PREFIX);
    out.text(line);
}
