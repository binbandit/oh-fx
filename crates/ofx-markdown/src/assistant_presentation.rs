use ofx_text::escape_terminal_controls;

use crate::presentation::ansi::{MAX_PIPE_BUFFER_BYTES, write_horizontal_rule};
use crate::presentation::block_parse::{
    BlockquotePrefix, CodeFence, ParsedFootnoteDefinition, closes_code_fence, code_fence_language,
    definition_marker_body, deindent_code_line, footnote_continuation_body,
    has_indented_code_prefix, is_blockquote_paragraph, is_horizontal_rule,
    is_lazy_blockquote_continuation, is_pipe_line, is_setext_candidate, is_valid_table,
    parse_blockquote, parse_code_fence, parse_footnote_definition, parse_header,
    parse_ordered_list, parse_setext_underline, parse_unordered_list, strip_fence_indent,
    table_lines,
};
use crate::presentation::block_render::{
    parse_table_payload_with_footnotes, render_code_block_payload as write_code_block_payload,
    render_table, render_table_payload as write_table_payload, table_output_stays_proportional,
    write_blockquote_line, write_code_line, write_definition_line, write_footnote_body,
    write_footnote_definition_marker, write_heading, write_list_line,
};
use crate::presentation::inline_render::write_inline;
use crate::presentation::payload::{CodeBlockPayload, FootnoteSink, TablePayload};
use crate::presentation::text_util::{
    is_blank_markdown_line, left_trim, without_terminal_hard_break_marker,
};
use crate::styled::{Line, TextOut};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Line(Line),
    Table(TablePayload),
    CodeBlock(CodeBlockPayload),
    ThematicRule,
}

impl Event {
    pub fn requires_text_drain(&self) -> bool {
        !matches!(self, Self::Line(_))
    }

    pub(crate) fn into_line(self) -> Option<Line> {
        match self {
            Self::Line(line) => Some(line),
            Self::Table(_) | Self::CodeBlock(_) | Self::ThematicRule => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Completions {
    pub(crate) tables: bool,
    pub(crate) code_blocks: bool,
    pub(crate) thematic_rules: bool,
}

impl Completions {
    pub const ALL: Self = Self {
        tables: true,
        code_blocks: true,
        thematic_rules: true,
    };
}

pub fn parse_table_payload(buf: &str) -> TablePayload {
    parse_table_payload_with_footnotes(buf, None)
}

pub fn render_table_payload(table: &TablePayload) -> Vec<Line> {
    collect_lines(|out| write_table_payload(table, out))
}

pub fn render_code_block_payload(block: &CodeBlockPayload) -> Vec<Line> {
    collect_lines(|out| write_code_block_payload(block, out))
}

fn collect_lines(write: impl FnOnce(&mut TextOut<'_>)) -> Vec<Line> {
    let mut events = Vec::new();
    let mut out = TextOut::new(&mut events);
    write(&mut out);
    out.finish();
    events.into_iter().filter_map(Event::into_line).collect()
}

#[derive(Clone, Copy, Debug)]
struct ActiveFootnote {
    index: usize,
    append_body: bool,
}

#[derive(Clone, Copy, Debug)]
struct CodeBlockState {
    fence: Option<CodeFence>,
}

#[derive(Clone, Copy, Debug)]
struct PipeBlockState {
    last_line_has_lf: bool,
}

#[derive(Clone, Debug)]
pub struct MarkdownProcessor {
    completions: Completions,
    line_buf: String,
    pending_top_level_line: String,
    pipe_buf: String,
    code_buf: String,
    code_language: String,
    code_block: Option<CodeBlockState>,
    pipe_block: Option<PipeBlockState>,
    active_blockquote: Option<BlockquotePrefix>,
    active_definition: bool,
    footnotes: FootnoteSink,
    active_footnote: Option<ActiveFootnote>,
    previous_line_was_blank: bool,
    trailing_newlines: usize,
}

impl MarkdownProcessor {
    pub fn with_completions(completions: Completions) -> Self {
        Self {
            completions,
            line_buf: String::new(),
            pending_top_level_line: String::new(),
            pipe_buf: String::new(),
            code_buf: String::new(),
            code_language: String::new(),
            code_block: None,
            pipe_block: None,
            active_blockquote: None,
            active_definition: false,
            footnotes: FootnoteSink::default(),
            active_footnote: None,
            previous_line_was_blank: true,
            trailing_newlines: 0,
        }
    }

    pub fn push(&mut self, input: &str, events: &mut Vec<Event>) {
        let first_new_event = events.len();
        let mut out = TextOut::new(events);
        let mut rest = input;
        while let Some(newline) = rest.find('\n') {
            self.line_buf.push_str(&rest[..newline]);
            rest = &rest[newline + 1..];
            let line = std::mem::take(&mut self.line_buf);
            let content = line.strip_suffix('\r').unwrap_or(&line);
            self.handle_line(content, true, &mut out);
            self.line_buf = line;
            self.line_buf.clear();
        }
        self.line_buf.push_str(rest);
        out.finish();
        self.trailing_newlines =
            trailing_newlines(&events[first_new_event..], self.trailing_newlines);
    }

    pub fn flush(&mut self, events: &mut Vec<Event>) {
        let first_new_event = events.len();
        let mut out = TextOut::new(events);
        let had_partial_line = !self.line_buf.is_empty();
        if had_partial_line {
            let line = std::mem::take(&mut self.line_buf);
            self.handle_line(&line, false, &mut out);
            self.line_buf = line;
            self.line_buf.clear();
        }
        self.flush_pending_top_level_line(&mut out);
        if had_partial_line && out.grew() && out.ends_with_newline() {
            out.pop_newline();
        }
        if self.pipe_block.is_some() {
            self.finalize_pipe_block(&mut out);
        }
        if self.code_block.is_some() {
            self.finalize_code_block(&mut out);
            self.code_block = None;
        }
        self.flush_footnotes(&mut out);
        self.active_blockquote = None;
        self.active_definition = false;
        self.active_footnote = None;
        out.finish();
        self.trailing_newlines =
            trailing_newlines(&events[first_new_event..], self.trailing_newlines);
    }

    fn handle_line(&mut self, line: &str, line_has_lf: bool, out: &mut TextOut<'_>) {
        self.handle_line_content(line, line_has_lf, out);
        self.previous_line_was_blank = is_blank_markdown_line(line);
    }

    fn handle_line_content(&mut self, line: &str, line_has_lf: bool, out: &mut TextOut<'_>) {
        if let Some(code_block) = self.code_block {
            self.handle_code_block_line(code_block, line, line_has_lf, out);
            return;
        }
        if self.pipe_block.is_some() && self.extend_pipe_block(line, line_has_lf, out) {
            return;
        }
        if self.extend_footnote(line) {
            return;
        }
        if let Some(definition) = parse_footnote_definition(line) {
            self.active_definition = false;
            self.flush_pending_top_level_line(out);
            self.begin_footnote_definition(&definition);
            return;
        }
        if self.completions.thematic_rules
            && !self.pending_top_level_line.is_empty()
            && self.resolve_pending_line(line, line_has_lf, out)
        {
            return;
        }
        if self.extend_blockquote(line, line_has_lf, out) {
            return;
        }
        if self.open_block(line, line_has_lf, out) {
            return;
        }
        self.handle_top_level_line(line, line_has_lf, out);
    }

    fn handle_code_block_line(
        &mut self,
        code_block: CodeBlockState,
        line: &str,
        line_has_lf: bool,
        out: &mut TextOut<'_>,
    ) {
        self.active_definition = false;
        if let Some(fence) = code_block.fence {
            if closes_code_fence(line, fence) {
                self.finalize_code_block(out);
                self.code_block = None;
                return;
            }
        } else if !is_blank_markdown_line(line) && !has_indented_code_prefix(line) {
            self.finalize_code_block(out);
            self.code_block = None;
            self.handle_line(line, line_has_lf, out);
            return;
        }
        let code_line = match code_block.fence {
            Some(fence) => strip_fence_indent(line, fence.indent),
            None if !is_blank_markdown_line(line) => deindent_code_line(line),
            None => line,
        };
        self.append_code_line(code_line, line_has_lf, out);
    }

    fn extend_pipe_block(&mut self, line: &str, line_has_lf: bool, out: &mut TextOut<'_>) -> bool {
        self.active_definition = false;
        if is_pipe_line(line) && self.pipe_buf.len() + line.len() < MAX_PIPE_BUFFER_BYTES {
            self.pipe_buf.push_str(line);
            self.pipe_buf.push('\n');
            self.pipe_block = Some(PipeBlockState {
                last_line_has_lf: line_has_lf,
            });
            return true;
        }
        self.finalize_pipe_block(out);
        false
    }

    fn extend_footnote(&mut self, line: &str) -> bool {
        let Some(active) = self.active_footnote else {
            return false;
        };
        if let Some(body) = footnote_continuation_body(line) {
            if active.append_body {
                let note = &mut self.footnotes.notes[active.index];
                note.body.push('\n');
                note.body.push_str(body);
            }
            return true;
        }
        self.active_footnote = None;
        false
    }

    fn resolve_pending_line(
        &mut self,
        line: &str,
        line_has_lf: bool,
        out: &mut TextOut<'_>,
    ) -> bool {
        if let Some(level) = parse_setext_underline(line) {
            self.active_definition = false;
            let pending = std::mem::take(&mut self.pending_top_level_line);
            write_heading(
                level,
                without_terminal_hard_break_marker(&pending, true),
                out,
                Some(&mut self.footnotes),
            );
            out.newline();
            self.pending_top_level_line = pending;
            self.pending_top_level_line.clear();
            return true;
        }
        if let Some(body) = definition_marker_body(line) {
            self.flush_pending_top_level_line(out);
            write_definition_line(body, line_has_lf, out, Some(&mut self.footnotes));
            self.active_definition = true;
            return true;
        }
        self.active_definition = false;
        self.flush_pending_top_level_line(out);
        false
    }

    fn extend_blockquote(&mut self, line: &str, line_has_lf: bool, out: &mut TextOut<'_>) -> bool {
        let Some(blockquote) = self.active_blockquote else {
            return false;
        };
        self.active_definition = false;
        if is_lazy_blockquote_continuation(line) {
            write_blockquote_line(
                blockquote,
                line,
                line_has_lf,
                out,
                Some(&mut self.footnotes),
            );
            out.newline();
            return true;
        }
        self.active_blockquote = None;
        false
    }

    fn open_block(&mut self, line: &str, line_has_lf: bool, out: &mut TextOut<'_>) -> bool {
        let indented_list_item = has_indented_code_prefix(line)
            && (parse_unordered_list(line).is_some() || parse_ordered_list(line).is_some());
        let plus_in_code_position =
            indented_list_item && self.previous_line_was_blank && left_trim(line).starts_with('+');
        if indented_list_item && !plus_in_code_position {
            self.active_definition = false;
            self.process_line(line, line_has_lf, out);
            out.newline();
            return true;
        }

        if self.previous_line_was_blank && has_indented_code_prefix(line) {
            self.active_definition = false;
            self.code_block = Some(CodeBlockState { fence: None });
            self.code_language.clear();
            self.append_code_line(deindent_code_line(line), line_has_lf, out);
            return true;
        }

        if is_pipe_line(line) {
            self.active_definition = false;
            self.pipe_block = Some(PipeBlockState {
                last_line_has_lf: line_has_lf,
            });
            self.pipe_buf.push_str(line);
            self.pipe_buf.push('\n');
            return true;
        }

        if let Some(fence) = parse_code_fence(line) {
            self.active_definition = false;
            self.code_block = Some(CodeBlockState { fence: Some(fence) });
            self.code_language.clear();
            self.code_language
                .push_str(&escape_terminal_controls(code_fence_language(line)));
            return true;
        }
        false
    }

    fn handle_top_level_line(&mut self, line: &str, line_has_lf: bool, out: &mut TextOut<'_>) {
        if self.active_definition
            && let Some(body) = definition_marker_body(line)
        {
            write_definition_line(body, line_has_lf, out, Some(&mut self.footnotes));
            return;
        }
        self.active_definition = false;

        if line_has_lf && self.completions.thematic_rules && is_setext_candidate(line) {
            self.pending_top_level_line.push_str(line);
            return;
        }

        if self.completions.thematic_rules && is_horizontal_rule(line) {
            out.push_event(Event::ThematicRule);
            return;
        }

        self.process_line(line, line_has_lf, out);
        out.newline();
    }

    fn append_code_line(&mut self, line: &str, line_has_lf: bool, out: &mut TextOut<'_>) {
        if self.completions.code_blocks {
            self.code_buf.push_str(line);
            self.code_buf.push('\n');
            return;
        }
        self.process_line(line, line_has_lf, out);
        out.newline();
    }

    fn flush_pending_top_level_line(&mut self, out: &mut TextOut<'_>) {
        if self.pending_top_level_line.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_top_level_line);
        self.process_line(&pending, true, out);
        out.newline();
        self.pending_top_level_line = pending;
        self.pending_top_level_line.clear();
    }

    fn process_line(&mut self, line: &str, line_has_lf: bool, out: &mut TextOut<'_>) {
        if self.code_block.is_some() {
            write_code_line(line, out);
            return;
        }
        if is_horizontal_rule(line) {
            write_horizontal_rule(out);
            return;
        }
        if let Some(header) = parse_header(without_terminal_hard_break_marker(line, line_has_lf)) {
            write_heading(header.level, header.content, out, Some(&mut self.footnotes));
            return;
        }
        if let Some(parsed) = parse_blockquote(line) {
            let blockquote = BlockquotePrefix {
                indent: parsed.indent,
                depth: parsed.depth,
            };
            self.active_blockquote = is_blockquote_paragraph(parsed.content).then_some(blockquote);
            write_blockquote_line(
                blockquote,
                parsed.content,
                line_has_lf,
                out,
                Some(&mut self.footnotes),
            );
            return;
        }
        if write_list_line(line, line_has_lf, out, Some(&mut self.footnotes)) {
            return;
        }
        write_inline(
            without_terminal_hard_break_marker(line, line_has_lf),
            out,
            false,
            Some(&mut self.footnotes),
        );
    }

    fn finalize_pipe_block(&mut self, out: &mut TextOut<'_>) {
        let buf = std::mem::take(&mut self.pipe_buf);
        let last_line_has_lf = self.pipe_block.is_some_and(|state| state.last_line_has_lf);
        if is_valid_table(&buf) && table_output_stays_proportional(&buf) {
            if self.completions.tables {
                let table = parse_table_payload_with_footnotes(&buf, Some(&mut self.footnotes));
                out.push_event(Event::Table(table));
            } else {
                render_table(&buf, out, Some(&mut self.footnotes));
            }
        } else {
            let mut lines = table_lines(&buf).peekable();
            while let Some(line) = lines.next() {
                let line_has_lf = lines.peek().is_some() || last_line_has_lf;
                self.process_line(line, line_has_lf, out);
                if line_has_lf {
                    out.newline();
                }
            }
        }
        self.pipe_buf = buf;
        self.pipe_buf.clear();
        self.pipe_block = None;
    }

    fn finalize_code_block(&mut self, out: &mut TextOut<'_>) {
        if self.completions.code_blocks {
            out.push_event(Event::CodeBlock(CodeBlockPayload {
                language: std::mem::take(&mut self.code_language),
                code: std::mem::take(&mut self.code_buf),
            }));
        }
        self.code_buf.clear();
        self.code_language.clear();
    }

    fn begin_footnote_definition(&mut self, definition: &ParsedFootnoteDefinition<'_>) {
        let index = self.footnotes.find_or_append(definition.label);
        let note = &mut self.footnotes.notes[index];
        if note.has_definition {
            self.active_footnote = Some(ActiveFootnote {
                index,
                append_body: false,
            });
            return;
        }
        note.body.push_str(definition.body);
        note.has_definition = true;
        self.active_footnote = Some(ActiveFootnote {
            index,
            append_body: true,
        });
    }

    fn flush_footnotes(&mut self, out: &mut TextOut<'_>) {
        if self.footnotes.has_numbered_definition() {
            while out.ends_with_newline() {
                out.pop_newline();
            }
            if out.has_text() {
                out.newline();
                out.newline();
            } else if trailing_newlines(out.new_events(), self.trailing_newlines) < 2 {
                out.newline();
            }

            let mut number = 1;
            while number <= self.footnotes.next_number {
                if let Some(body) = self.footnotes.defined_body(number) {
                    write_footnote_definition_marker(out, number);
                    write_footnote_body(&body, out, &mut self.footnotes, number);
                }
                number += 1;
            }
        }
        self.footnotes = FootnoteSink::default();
    }
}

fn trailing_newlines(events: &[Event], before: usize) -> usize {
    let mut newlines = 0;
    for event in events.iter().rev() {
        let Event::Line(line) = event else {
            return (newlines + 1).min(2);
        };
        newlines += usize::from(line.newline);
        if !line.is_empty() || !line.newline {
            return newlines.min(2);
        }
    }
    (before + newlines).min(2)
}

#[cfg(test)]
mod tests;
