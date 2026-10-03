use memchr::memchr;

use crate::presentation::code_highlight_languages::{BlockComment, Profile};
use crate::styled::{Hang, Line, Slot, SpanWriter};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffMarkers {
    Unstyled,
    Accented,
}

const COMMAND_PREFIXES: [&str; 7] = ["if", "then", "elif", "else", "while", "until", "do"];

struct Highlighted {
    lines: Vec<Line>,
    writer: SpanWriter,
    base: Option<Slot>,
}

impl Highlighted {
    fn new(base: Option<Slot>) -> Self {
        Self {
            lines: Vec::new(),
            writer: SpanWriter::default(),
            base,
        }
    }

    fn plain(&mut self, text: &str) {
        self.emit(self.base, text);
    }

    fn styled(&mut self, slot: Slot, text: &str) {
        self.emit(Some(slot), text);
    }

    fn emit(&mut self, slot: Option<Slot>, text: &str) {
        self.writer.set_slot(slot);
        let mut segments = text.split('\n');
        if let Some(first) = segments.next() {
            self.writer.text(first);
        }
        for segment in segments {
            self.newline();
            self.writer.set_slot(slot);
            self.writer.text(segment);
        }
    }

    fn newline(&mut self) {
        self.lines.push(self.writer.take_line(true));
    }

    fn finish(mut self) -> Vec<Line> {
        self.lines.push(self.writer.take_line(false));
        self.lines
    }
}

pub fn highlight(source: &str, profile: &Profile, base: Option<Slot>) -> Vec<Line> {
    let mut highlighter = Highlighter {
        out: Highlighted::new(base),
        source,
        profile,
        command_position: profile.command_words(),
        close_braces: CloseBraceSearch::default(),
    };
    let mut index = 0;
    while index < source.len() {
        index = highlighter.step(index);
    }
    highlighter.out.finish()
}

struct Highlighter<'a> {
    out: Highlighted,
    source: &'a str,
    profile: &'a Profile,
    command_position: bool,
    close_braces: CloseBraceSearch,
}

#[derive(Clone, Copy, Default)]
struct CloseBraceSearch {
    searched_from: Option<usize>,
    found: Option<usize>,
}

impl CloseBraceSearch {
    fn find(&mut self, bytes: &[u8], from: usize) -> Option<usize> {
        let cached = self.searched_from.is_some_and(|searched_from| {
            searched_from <= from && self.found.is_none_or(|found| from <= found)
        });
        if !cached {
            self.searched_from = Some(from);
            self.found =
                memchr(b'}', bytes.get(from..).unwrap_or_default()).map(|offset| from + offset);
        }
        self.found
    }
}

impl Highlighter<'_> {
    fn step(&mut self, index: usize) -> usize {
        let source = self.source;
        let bytes = source.as_bytes();
        let byte = bytes[index];
        if byte == b'\n' {
            self.out.newline();
            self.command_position = self.profile.command_words();
            return index + 1;
        }
        if let Some(end) = block_comment_end(source, index, self.profile.block_comment())
            .or_else(|| line_comment_end(source, index, self.profile.line_comments()))
        {
            return self.token(Slot::SyntaxComment, index, end);
        }
        if self.profile.quotes().contains(&byte) {
            let end = quoted_end(bytes, index);
            if byte == b'"' && self.profile.dollar_vars() {
                append_double_quoted(&mut self.out, &source[index..end]);
                self.command_position = false;
                return end;
            }
            return self.token(Slot::SyntaxString, index, end);
        }
        if is_number_start(bytes, index) {
            let end = number_end(bytes, index);
            if self.profile.bare_numbers() || fd_context(bytes, index, end) {
                return self.token(Slot::SyntaxNumber, index, end);
            }
            self.out.plain(&source[index..end]);
            self.command_position = false;
            return end;
        }
        if let Some(end) = self.shell_word(index) {
            return end;
        }
        if self.profile.operators().contains(&byte) {
            let end = operator_run_end(bytes, index, self.profile.operators());
            let run = &source[index..end];
            self.out.styled(Slot::SyntaxOperator, run);
            self.command_position = !run.contains(['<', '>']);
            return end;
        }
        if self.profile.command_words() && byte == b'`' {
            self.out.plain("`");
            self.command_position = true;
            return index + 1;
        }
        if is_identifier_start(byte) {
            return self.identifier(index);
        }
        let character_len = source[index..].chars().next().map_or(1, char::len_utf8);
        self.out.plain(&source[index..index + character_len]);
        if !is_ascii_whitespace(byte) {
            self.command_position = false;
        }
        index + character_len
    }

    fn token(&mut self, slot: Slot, start: usize, end: usize) -> usize {
        self.out.styled(slot, &self.source[start..end]);
        self.command_position = false;
        end
    }

    fn shell_word(&mut self, index: usize) -> Option<usize> {
        let bytes = self.source.as_bytes();
        let byte = bytes[index];
        if self.profile.dollar_vars() && byte == b'$' {
            if bytes.get(index + 1) == Some(&b'(') {
                self.out.styled(Slot::SyntaxOperator, "$(");
                self.command_position = true;
                return Some(index + 2);
            }
            if let Some(end) = dollar_var_end(bytes, index, bytes.len(), &mut self.close_braces) {
                return Some(self.token(Slot::SyntaxVariable, index, end));
            }
        }
        if self.profile.dollar_vars()
            && byte == b'~'
            && tilde_start(bytes, index, self.profile.operators())
        {
            return Some(self.token(Slot::SyntaxVariable, index, index + 1));
        }
        if self.profile.dash_flags() && byte == b'-' {
            let end = flag_end(bytes, index, self.profile.operators())?;
            return Some(self.token(Slot::SyntaxNumber, index, end));
        }
        None
    }

    fn identifier(&mut self, index: usize) -> usize {
        let bytes = self.source.as_bytes();
        let end = identifier_end(bytes, index);
        let token = &self.source[index..end];
        let after_separator = index > 0 && bytes[index - 1] == b'/';
        let slot = word_slot(self.profile, token, self.command_position, after_separator);
        match slot {
            Some(slot) => self.out.styled(slot, token),
            None => self.out.plain(token),
        }
        if self.profile.command_words() {
            self.command_position =
                slot.is_some() && self.command_position && COMMAND_PREFIXES.contains(&token);
        }
        end
    }
}

fn word_slot(
    profile: &Profile,
    token: &str,
    command_position: bool,
    after_separator: bool,
) -> Option<Slot> {
    if profile.command_words() {
        if !command_position || after_separator {
            return None;
        }
        return Some(if COMMAND_PREFIXES.contains(&token) {
            Slot::SyntaxKeyword
        } else {
            Slot::SyntaxFunction
        });
    }
    if after_separator {
        return None;
    }
    if profile.keywords().contains(token, profile.keyword_case()) {
        Some(Slot::SyntaxKeyword)
    } else if profile.literals().contains(token, profile.keyword_case()) {
        Some(Slot::SyntaxNumber)
    } else {
        None
    }
}

pub fn highlight_diff(source: &str, markers: DiffMarkers) -> Vec<Line> {
    let mut out = Highlighted::new(None);
    let mut lines = source.split('\n').peekable();
    while let Some(line) = lines.next() {
        out.emit(diff_line_slot(line, markers), line);
        if lines.peek().is_some() {
            out.newline();
        }
    }
    let mut highlighted = out.finish();
    for line in &mut highlighted {
        line.hang = Hang::None;
    }
    highlighted
}

fn diff_line_slot(line: &str, markers: DiffMarkers) -> Option<Slot> {
    let accented = markers == DiffMarkers::Accented;
    if line.starts_with("+++") || line.starts_with("---") {
        Some(Slot::SyntaxComment)
    } else if line.starts_with('+') {
        accented.then_some(Slot::DiffAddedMarker)
    } else if line.starts_with('-') {
        accented.then_some(Slot::DiffRemovedMarker)
    } else if line.starts_with("@@") {
        Some(Slot::SyntaxKeyword)
    } else if [
        "diff ",
        "index ",
        "new file",
        "deleted file",
        "similarity",
        "rename ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
    {
        Some(Slot::SyntaxComment)
    } else {
        None
    }
}

fn append_double_quoted(out: &mut Highlighted, text: &str) {
    let bytes = text.as_bytes();
    let mut close_braces = CloseBraceSearch::default();
    let inner_end = text
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index);
    let mut chunk_start = 0;
    let mut index = 1;
    while index < inner_end {
        if bytes[index] == b'$' && bytes[index - 1] != b'\\' {
            let variable_end = if index + 1 < inner_end && bytes[index + 1] == b'(' {
                Some(index + 2)
            } else {
                dollar_var_end(bytes, index, inner_end, &mut close_braces)
            };
            if let Some(end) = variable_end {
                if chunk_start < index {
                    out.styled(Slot::SyntaxString, &text[chunk_start..index]);
                }
                out.styled(Slot::SyntaxVariable, &text[index..end]);
                index = end;
                chunk_start = end;
                continue;
            }
        }
        index += 1;
    }
    if chunk_start < inner_end {
        out.styled(Slot::SyntaxString, &text[chunk_start..inner_end]);
    }
    out.styled(Slot::SyntaxString, &text[inner_end..]);
}

fn fd_context(bytes: &[u8], start: usize, end: usize) -> bool {
    if matches!(bytes.get(end), Some(b'>' | b'<')) {
        return true;
    }
    if start > 0 && matches!(bytes[start - 1], b'>' | b'<') {
        return true;
    }
    start > 1 && bytes[start - 1] == b'&' && matches!(bytes[start - 2], b'>' | b'<')
}

fn tilde_start(bytes: &[u8], index: usize, operators: &[u8]) -> bool {
    if index > 0 {
        let previous = bytes[index - 1];
        if !is_ascii_whitespace(previous)
            && !operators.contains(&previous)
            && previous != b'('
            && previous != b'`'
        {
            return false;
        }
    }
    bytes
        .get(index + 1)
        .is_some_and(|&next| next == b'/' || is_identifier_start(next) || next.is_ascii_digit())
}

fn block_comment_end(source: &str, index: usize, comment: Option<BlockComment>) -> Option<usize> {
    let comment = comment?;
    if !source.as_bytes()[index..].starts_with(comment.start.as_bytes()) {
        return None;
    }
    let content_start = index + comment.start.len();
    Some(
        source[content_start..]
            .find(comment.end)
            .map_or(source.len(), |offset| {
                content_start + offset + comment.end.len()
            }),
    )
}

fn line_comment_end(
    source: &str,
    index: usize,
    prefixes: impl Iterator<Item = &'static str>,
) -> Option<usize> {
    let rest = &source.as_bytes()[index..];
    if !prefixes
        .into_iter()
        .any(|prefix| rest.starts_with(prefix.as_bytes()))
    {
        return None;
    }
    Some(
        rest.iter()
            .position(|&byte| byte == b'\n')
            .map_or(source.len(), |offset| index + offset),
    )
}

fn quoted_end(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == b'\n' {
            return index;
        }
        if bytes[index] == b'\\' && index + 1 < bytes.len() {
            index += 2;
            continue;
        }
        if bytes[index] == quote {
            return index + 1;
        }
        index += 1;
    }
    bytes.len()
}

fn is_number_start(bytes: &[u8], index: usize) -> bool {
    if !bytes[index].is_ascii_digit() {
        return false;
    }
    if index == 0 {
        return true;
    }
    let previous = bytes[index - 1];
    if is_identifier_continue(previous) {
        return false;
    }
    !(previous == b'-' && index >= 2 && is_identifier_continue(bytes[index - 2]))
}

fn number_end(bytes: &[u8], start: usize) -> usize {
    start
        + bytes[start..]
            .iter()
            .take_while(|&&byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_')
            .count()
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$'
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

fn identifier_end(bytes: &[u8], start: usize) -> usize {
    start
        + 1
        + bytes[start + 1..]
            .iter()
            .take_while(|&&byte| is_identifier_continue(byte))
            .count()
}

fn operator_run_end(bytes: &[u8], start: usize, operators: &[u8]) -> usize {
    start
        + bytes[start..]
            .iter()
            .take_while(|byte| operators.contains(byte))
            .count()
}

fn dollar_var_end(
    bytes: &[u8],
    start: usize,
    limit: usize,
    close_braces: &mut CloseBraceSearch,
) -> Option<usize> {
    let next = start + 1;
    if next >= limit {
        return None;
    }
    let byte = bytes[next];
    if byte == b'{' {
        let close = close_braces.find(bytes, next + 1)?;
        return (close < limit).then_some(close + 1);
    }
    if byte.is_ascii_alphabetic() || byte == b'_' {
        return Some(
            next + bytes[next..limit]
                .iter()
                .take_while(|&&candidate| is_identifier_continue(candidate))
                .count(),
        );
    }
    if byte.is_ascii_digit() || b"?#@*!$".contains(&byte) {
        return Some(next + 1);
    }
    None
}

fn flag_end(bytes: &[u8], start: usize, operators: &[u8]) -> Option<usize> {
    if start > 0 {
        let previous = bytes[start - 1];
        if !is_ascii_whitespace(previous) && !operators.contains(&previous) {
            return None;
        }
    }
    let next = start + 1;
    let byte = *bytes.get(next)?;
    if !byte.is_ascii_alphanumeric() && byte != b'-' {
        return None;
    }
    Some(
        next + bytes[next..]
            .iter()
            .take_while(|&&candidate| candidate.is_ascii_alphanumeric() || candidate == b'-')
            .count(),
    )
}

fn is_ascii_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::code_highlight_languages::resolve;

    fn profile(label: &str) -> &'static Profile {
        resolve(label).expect("registered profile")
    }

    fn plain(lines: &[Line]) -> String {
        lines.iter().map(Line::text).collect::<Vec<_>>().join("\n")
    }

    fn spans(lines: &[Line]) -> Vec<(String, Option<Slot>)> {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| (span.text.clone(), span.style.slot))
            .collect()
    }

    fn has(lines: &[Line], text: &str, slot: Option<Slot>) -> bool {
        spans(lines)
            .iter()
            .any(|(span_text, span_slot)| span_text == text && *span_slot == slot)
    }

    fn count(lines: &[Line], text: &str, slot: Option<Slot>) -> usize {
        spans(lines)
            .iter()
            .filter(|(span_text, span_slot)| span_text == text && *span_slot == slot)
            .count()
    }

    fn styled(lines: &[Line]) -> bool {
        spans(lines).iter().any(|(_, slot)| slot.is_some())
    }

    #[test]
    fn supported_source_gains_balanced_colors_without_changing_code_bytes() {
        let source = "const value = \"const\"; // return\n";
        let lines = highlight(source, profile("zig"), None);
        assert_eq!(plain(&lines), source);
        assert_eq!(count(&lines, "const", Some(Slot::SyntaxKeyword)), 1);
        assert!(has(&lines, "\"const\"", Some(Slot::SyntaxString)));
        assert!(has(&lines, "// return", Some(Slot::SyntaxComment)));
        assert!(!has(&lines, "return", Some(Slot::SyntaxKeyword)));
        assert_eq!(lines.len(), 2);
        assert!(lines[0].newline && !lines[1].newline && lines[1].is_empty());
    }

    #[test]
    fn every_registered_profile_highlights_representative_source() {
        let cases = [
            ("zig", "pub fn main() void { return; }"),
            ("ts", "const ready = true;"),
            ("json", "{\"ready\": true}"),
            ("sh", "if true; then echo \"ready\"; fi"),
            ("python", "def ready(): return True"),
            ("yaml", "ready: true # comment"),
            ("toml", "ready = true # comment"),
            ("sql", "SELECT id FROM users"),
            ("dockerfile", "FROM alpine:3.20"),
            ("rust", "fn main() { let ready = true; }"),
            ("go", "package main\nfunc main() {}"),
            ("c", "int main(void) { return 0; }"),
            ("cpp", "class Ready { public: bool value = true; };"),
            ("csharp", "public class Ready { }"),
            ("java", "public class Ready { }"),
            ("kotlin", "fun ready(): Boolean = true"),
            ("php", "<?php function ready() { return true; }"),
            ("ruby", "def ready\n  true\nend"),
            ("swift", "func ready() -> Bool { true }"),
            ("powershell", "Function Ready { return $true }"),
            ("lua", "local ready = true"),
            ("html", "<main class=\"ready\"></main>"),
            ("xml", "<?xml version=\"1.0\"?>"),
            ("css", ".ready { color: red; }"),
            ("hcl", "resource \"ready\" \"main\" {}"),
        ];
        for (label, source) in cases {
            let lines = highlight(source, profile(label), None);
            assert_eq!(plain(&lines), source, "{label}");
            assert!(styled(&lines), "{label}");
        }
    }

    #[test]
    fn profiles_use_configured_block_comments_and_case_insensitive_keywords() {
        let css = highlight("/* comment */", profile("css"), None);
        let sql = highlight("SELECT id FROM users", profile("sql"), None);
        let html = highlight("<!-- note -->", profile("html"), None);
        assert!(has(&css, "/* comment */", Some(Slot::SyntaxComment)));
        assert!(has(&sql, "SELECT", Some(Slot::SyntaxKeyword)));
        assert!(has(&html, "<!-- note -->", Some(Slot::SyntaxComment)));
    }

    #[test]
    fn base_style_wraps_the_span_and_restores_after_each_token() {
        let lines = highlight("echo 'hi there' 42", profile("sh"), Some(Slot::Dim));
        assert_eq!(
            spans(&lines),
            [
                ("echo".to_owned(), Some(Slot::SyntaxFunction)),
                (" ".to_owned(), Some(Slot::Dim)),
                ("'hi there'".to_owned(), Some(Slot::SyntaxString)),
                (" 42".to_owned(), Some(Slot::Dim)),
            ]
        );
    }

    #[test]
    fn shell_operators_and_variables_take_the_keyword_color() {
        let lines = highlight(
            "cd /tmp && echo $HOME | head -2 > out; echo $? # done",
            profile("sh"),
            None,
        );
        for operator in ["&&", "|", ">", ";"] {
            assert!(
                has(&lines, operator, Some(Slot::SyntaxOperator)),
                "{operator}"
            );
        }
        assert!(has(&lines, "$HOME", Some(Slot::SyntaxVariable)));
        assert!(has(&lines, "$?", Some(Slot::SyntaxVariable)));
        assert!(has(&lines, "# done", Some(Slot::SyntaxComment)));
        assert!(has(&lines, "-2", Some(Slot::SyntaxNumber)));

        let quoted = highlight("echo '$HOME'", profile("sh"), None);
        assert!(has(&quoted, "'$HOME'", Some(Slot::SyntaxString)));

        let zig_source = highlight("a < b", profile("zig"), None);
        assert!(!styled(&zig_source));
        assert_eq!(plain(&zig_source), "a < b");
    }

    #[test]
    fn digit_runs_glued_to_words_by_a_dash_stay_plain() {
        let lines = highlight(
            "cd build-20260918 && head -80 2>/dev/null",
            profile("sh"),
            None,
        );
        assert!(has(&lines, " build-20260918 ", None));
        assert!(has(&lines, "/dev/null", None));
        assert!(has(&lines, "-80", Some(Slot::SyntaxNumber)));
        assert!(has(&lines, "2", Some(Slot::SyntaxNumber)));
    }

    #[test]
    fn dash_flags_color_as_units_only_at_word_boundaries() {
        let lines = highlight("tail -8 --json && cat - < in", profile("sh"), None);
        assert!(has(&lines, "-8", Some(Slot::SyntaxNumber)));
        assert!(has(&lines, "--json", Some(Slot::SyntaxNumber)));
        let tail: Vec<_> = spans(&lines).into_iter().rev().take(4).collect();
        assert_eq!(
            tail,
            [
                (" in".to_owned(), None),
                ("<".to_owned(), Some(Slot::SyntaxOperator)),
                (" - ".to_owned(), None),
                ("cat".to_owned(), Some(Slot::SyntaxFunction)),
            ]
        );

        let after_pipe = highlight("echo x | head -1", profile("sh"), None);
        assert!(has(&after_pipe, "-1", Some(Slot::SyntaxNumber)));

        let zig_source = highlight("a - b", profile("zig"), None);
        assert!(!styled(&zig_source));
    }

    #[test]
    fn command_position_colors_any_command_word_and_only_command_words() {
        let lines = highlight("gh run list | xargs echo > out.txt", profile("sh"), None);
        assert!(has(&lines, "gh", Some(Slot::SyntaxFunction)));
        assert!(has(&lines, "xargs", Some(Slot::SyntaxFunction)));
        assert!(has(&lines, " echo ", None));
        assert!(has(&lines, " out.txt", None));

        let chain = highlight("if cd /x; then echo hi; fi", profile("sh"), None);
        assert!(has(&chain, "if", Some(Slot::SyntaxKeyword)));
        assert!(has(&chain, "cd", Some(Slot::SyntaxFunction)));
        assert!(has(&chain, "then", Some(Slot::SyntaxKeyword)));
        assert!(has(&chain, "echo", Some(Slot::SyntaxFunction)));
        assert!(has(&chain, "fi", Some(Slot::SyntaxFunction)));
    }

    #[test]
    fn bare_number_arguments_stay_plain_but_redirect_fds_color() {
        let lines = highlight("sleep 5; exit 7 2>&1", profile("sh"), None);
        assert!(has(&lines, " 5", None));
        assert!(has(&lines, " 7 ", None));
        assert!(has(&lines, "2", Some(Slot::SyntaxNumber)));
        assert!(has(&lines, "1", Some(Slot::SyntaxNumber)));
    }

    #[test]
    fn braced_variables_tildes_globs_and_substitution_parse_like_the_grammar() {
        let lines = highlight(
            "cp ${SRC}/*.log ~/out && echo $(date +%F)",
            profile("sh"),
            None,
        );
        assert!(has(&lines, "${SRC}", Some(Slot::SyntaxVariable)));
        assert!(has(&lines, "*", Some(Slot::SyntaxOperator)));
        assert!(has(&lines, "~", Some(Slot::SyntaxVariable)));
        assert!(has(&lines, "/out ", None));
        assert!(has(&lines, "$(", Some(Slot::SyntaxOperator)));
        assert!(has(&lines, "date", Some(Slot::SyntaxFunction)));

        let ticks = highlight("echo `uname -s`", profile("sh"), None);
        assert!(has(&ticks, " `", None));
        assert!(has(&ticks, "uname", Some(Slot::SyntaxFunction)));
        assert!(has(&ticks, "-s", Some(Slot::SyntaxNumber)));
    }

    #[test]
    fn double_quotes_interpolate_variables_inside_the_string_color() {
        let lines = highlight("echo \"hi $USER from ${HOME}\"", profile("sh"), None);
        assert!(has(&lines, "\"hi ", Some(Slot::SyntaxString)));
        assert!(has(&lines, "$USER", Some(Slot::SyntaxVariable)));
        assert!(has(&lines, "${HOME}", Some(Slot::SyntaxVariable)));

        let single = highlight("echo '$USER'", profile("sh"), None);
        assert!(has(&single, "'$USER'", Some(Slot::SyntaxString)));
    }

    #[test]
    fn diff_lines_paint_with_the_caller_s_marker_colors() {
        let patch = "diff --git a/f b/f\nindex 111..222 100644\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-old line\n+new line\n context";
        let lines = highlight_diff(patch, DiffMarkers::Accented);
        assert!(has(&lines, "+new line", Some(Slot::DiffAddedMarker)));
        assert!(has(&lines, "-old line", Some(Slot::DiffRemovedMarker)));
        assert!(has(&lines, "@@ -1,2 +1,2 @@", Some(Slot::SyntaxKeyword)));
        assert!(has(&lines, "--- a/f", Some(Slot::SyntaxComment)));
        assert!(has(&lines, "diff --git a/f b/f", Some(Slot::SyntaxComment)));
        assert!(has(&lines, " context", None));
        assert_eq!(plain(&lines), patch);

        let unstyled = highlight_diff(patch, DiffMarkers::Unstyled);
        assert!(has(&unstyled, "+new line", None));
        assert!(has(&unstyled, "-old line", None));
    }

    #[test]
    fn text_blocks_stay_byte_identical_and_markdown_colors_inline_code() {
        let text = highlight(
            "plain prose with 42 numbers and # no comment",
            profile("text"),
            None,
        );
        assert!(!styled(&text));
        assert_eq!(plain(&text), "plain prose with 42 numbers and # no comment");

        let markdown = highlight("run `fx upgrade` to update", profile("md"), None);
        assert!(has(&markdown, "`fx upgrade`", Some(Slot::SyntaxString)));
    }

    #[test]
    fn split_slots_let_themes_color_commands_variables_and_operators_apart() {
        let lines = highlight(
            "while true; do echo $HOME | head -2; done",
            profile("sh"),
            None,
        );
        assert!(has(&lines, "while", Some(Slot::SyntaxKeyword)));
        assert!(has(&lines, "true", Some(Slot::SyntaxFunction)));
        assert!(has(&lines, "echo", Some(Slot::SyntaxFunction)));
        assert!(has(&lines, "$HOME", Some(Slot::SyntaxVariable)));
        assert!(has(&lines, "|", Some(Slot::SyntaxOperator)));
    }

    #[test]
    fn highlighted_source_reaches_spans_with_terminal_controls_escaped() {
        let source = "echo \"\x1b]0;t\x07\" \u{9b}2J # \u{202e}x\n";
        let lines = highlight(source, profile("sh"), None);
        assert_eq!(
            plain(&lines),
            "echo \"\\x1b]0;t\\x07\" \\u{009b}2J # \\u{202e}x\n"
        );
        let diff = highlight_diff("+\x1b[2J\n-\u{85}", DiffMarkers::Accented);
        assert_eq!(plain(&diff), "+\\x1b[2J\n-\\u{0085}");
    }

    #[test]
    fn unclosed_braced_variables_highlight_in_linear_time() {
        let sh = profile("sh");
        for source in ["${".repeat(60_000), format!("\"{}", "${".repeat(60_000))] {
            let started = std::time::Instant::now();
            let lines = highlight(&source, sh, None);
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "highlighting took {:?}",
                started.elapsed()
            );
            assert_eq!(plain(&lines), source);
        }
    }

    #[test]
    fn block_comments_spanning_lines_keep_their_slot_on_every_line() {
        let lines = highlight("a /* one\ntwo */ b", profile("c"), None);
        assert_eq!(lines.len(), 2);
        assert!(has(&lines, "/* one", Some(Slot::SyntaxComment)));
        assert!(has(&lines, "two */", Some(Slot::SyntaxComment)));
        assert!(has(&lines, " b", None));
    }
}
