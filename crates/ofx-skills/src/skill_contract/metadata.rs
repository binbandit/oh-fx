use super::{MAX_FRONTMATTER_BYTES, MAX_NAME_BYTES};
use crate::byte_trim::{trim, trim_start};

const BLANK: &[u8] = b" \t";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidMetadataCause {
    FrontmatterTooLong,
    MissingClosingDelimiter,
    MissingName,
    DuplicateRecognizedKey,
    InvalidName,
    NameTooLong,
    MalformedQuote,
    UnsupportedMultiline,
    InvalidUtf8,
    ControlByte,
}

impl InvalidMetadataCause {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::FrontmatterTooLong => "frontmatter_too_long",
            Self::MissingClosingDelimiter => "missing_closing_delimiter",
            Self::MissingName => "missing_name",
            Self::DuplicateRecognizedKey => "duplicate_recognized_key",
            Self::InvalidName => "invalid_name",
            Self::NameTooLong => "name_too_long",
            Self::MalformedQuote => "malformed_quote",
            Self::UnsupportedMultiline => "unsupported_multiline",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::ControlByte => "control_byte",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetadataStatus {
    NoFrontmatter,
    Valid,
    Invalid(InvalidMetadataCause),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockStyle {
    FoldedClip,
    FoldedStrip,
    LiteralClip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockDescription {
    style: BlockStyle,
    base_indent: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedSkillFile<'a> {
    pub(crate) name: Option<&'a [u8]>,
    pub(crate) description: Option<&'a [u8]>,
    pub(crate) body: &'a [u8],
    pub(crate) status: MetadataStatus,
    description_block: Option<BlockDescription>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillMetadata {
    pub(crate) name: String,
    pub(crate) description: String,
}

pub(crate) fn resolve_metadata(
    parsed: &ParsedSkillFile<'_>,
    fallback_name: &[u8],
) -> Result<SkillMetadata, InvalidMetadataCause> {
    match parsed.status {
        MetadataStatus::NoFrontmatter => {
            if let Some(cause) = invalid_skill_name_cause(fallback_name) {
                return Err(cause);
            }
            Ok(SkillMetadata {
                name: utf8_text(fallback_name)?,
                description: String::new(),
            })
        }
        MetadataStatus::Invalid(cause) => Err(cause),
        MetadataStatus::Valid => {
            let name = parsed.name.ok_or(InvalidMetadataCause::MissingName)?;
            let raw_description = parsed.description.unwrap_or_default();
            let description = match parsed.description_block {
                Some(block) => decode_block_description(raw_description, block),
                None => raw_description.to_vec(),
            };
            Ok(SkillMetadata {
                name: utf8_text(name)?,
                description: String::from_utf8(description)
                    .map_err(|_| InvalidMetadataCause::InvalidUtf8)?,
            })
        }
    }
}

fn utf8_text(bytes: &[u8]) -> Result<String, InvalidMetadataCause> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| InvalidMetadataCause::InvalidUtf8)
}

pub(crate) fn parse_skill_file(content: &[u8]) -> ParsedSkillFile<'_> {
    let unparsed = |status| ParsedSkillFile {
        name: None,
        description: None,
        body: content,
        status,
        description_block: None,
    };
    let Some(header_start) = frontmatter_header_start(content) else {
        return unparsed(MetadataStatus::NoFrontmatter);
    };
    let searched = &content[..content.len().min(MAX_FRONTMATTER_BYTES + 1)];
    let Some(closing) = find_closing_delimiter(searched, header_start) else {
        let cause = if content.len() > MAX_FRONTMATTER_BYTES {
            InvalidMetadataCause::FrontmatterTooLong
        } else {
            InvalidMetadataCause::MissingClosingDelimiter
        };
        return unparsed(MetadataStatus::Invalid(cause));
    };
    if closing.body_start > MAX_FRONTMATTER_BYTES {
        return unparsed(MetadataStatus::Invalid(
            InvalidMetadataCause::FrontmatterTooLong,
        ));
    }

    let header = &content[header_start..closing.header_end];
    let body = trim_start(&content[closing.body_start..], b"\r\n");
    let mut fields = HeaderFields::default();
    fields.scan(header);
    fields.finish(body)
}

#[derive(Default)]
struct HeaderFields<'a> {
    name: Option<&'a [u8]>,
    description: Option<&'a [u8]>,
    description_block: Option<BlockDescription>,
    invalid_cause: Option<InvalidMetadataCause>,
    saw_name: bool,
    saw_description: bool,
}

impl<'a> HeaderFields<'a> {
    fn invalidate(&mut self, cause: InvalidMetadataCause) {
        self.invalid_cause.get_or_insert(cause);
    }

    fn scan(&mut self, header: &'a [u8]) {
        let mut previous_line_recognized = false;
        let mut line_offset = 0;
        while let Some(line) = header_line_at(header, line_offset) {
            line_offset = line.next;
            let trimmed = trim(line.bytes, BLANK);
            if trimmed.is_empty() || trimmed[0] == b'#' {
                continue;
            }
            if previous_line_recognized && matches!(line.bytes.first(), Some(b' ' | b'\t')) {
                self.invalidate(InvalidMetadataCause::UnsupportedMultiline);
                previous_line_recognized = false;
                continue;
            }
            let Some(colon) = trimmed.iter().position(|&byte| byte == b':') else {
                previous_line_recognized = false;
                continue;
            };
            let key = trim(&trimmed[..colon], BLANK);
            let raw_value = trim(&trimmed[colon + 1..], BLANK);
            previous_line_recognized = match key {
                b"name" => {
                    self.record_name(raw_value);
                    true
                }
                b"description" => {
                    if let Some(style) = block_style(raw_value) {
                        line_offset = self.record_block_description(header, line_offset, style);
                        false
                    } else {
                        self.record_inline_description(raw_value);
                        true
                    }
                }
                _ => false,
            };
        }
    }

    fn record_name(&mut self, raw_value: &'a [u8]) {
        if self.saw_name {
            self.invalidate(InvalidMetadataCause::DuplicateRecognizedKey);
        }
        self.saw_name = true;
        let (value, cause) = parse_recognized_value(raw_value);
        self.name = Some(value);
        if let Some(cause) = cause {
            self.invalidate(cause);
        }
    }

    fn mark_description(&mut self) {
        if self.saw_description {
            self.invalidate(InvalidMetadataCause::DuplicateRecognizedKey);
        }
        self.saw_description = true;
    }

    fn record_inline_description(&mut self, raw_value: &'a [u8]) {
        self.mark_description();
        let (value, cause) = parse_recognized_value(raw_value);
        self.description = Some(value);
        self.description_block = None;
        if let Some(cause) = cause {
            self.invalidate(cause);
        }
    }

    fn record_block_description(
        &mut self,
        header: &'a [u8],
        start: usize,
        style: BlockStyle,
    ) -> usize {
        self.mark_description();
        let block = parse_block_description(header, start, style);
        self.description = Some(block.value);
        self.description_block = Some(block.description);
        if let Some(cause) = block.invalid_cause {
            self.invalidate(cause);
        }
        block.next_offset
    }

    fn finish(mut self, body: &'a [u8]) -> ParsedSkillFile<'a> {
        match self.name {
            Some(name) => {
                if let Some(cause) = invalid_skill_name_cause(name) {
                    self.invalidate(cause);
                }
            }
            None => self.invalidate(InvalidMetadataCause::MissingName),
        }
        if self.description_block.is_none()
            && let Some(cause) = self.description.and_then(invalid_text_cause)
        {
            self.invalidate(cause);
        }
        let status = match self.invalid_cause {
            Some(cause) => MetadataStatus::Invalid(cause),
            None => MetadataStatus::Valid,
        };
        ParsedSkillFile {
            name: self.name,
            description: self.description,
            body,
            status,
            description_block: self
                .description_block
                .filter(|_| self.invalid_cause.is_none()),
        }
    }
}

struct HeaderLine<'a> {
    start: usize,
    bytes: &'a [u8],
    next: usize,
}

fn header_line_at(header: &[u8], start: usize) -> Option<HeaderLine<'_>> {
    if start >= header.len() {
        return None;
    }
    let newline = header[start..].iter().position(|&byte| byte == b'\n');
    let line_end = newline.map_or(header.len(), |offset| start + offset);
    let raw_line = &header[start..line_end];
    Some(HeaderLine {
        start,
        bytes: raw_line.strip_suffix(b"\r").unwrap_or(raw_line),
        next: if newline.is_some() {
            line_end + 1
        } else {
            line_end
        },
    })
}

struct ParsedBlockDescription<'a> {
    value: &'a [u8],
    description: BlockDescription,
    next_offset: usize,
    invalid_cause: Option<InvalidMetadataCause>,
}

fn parse_block_description(
    header: &[u8],
    start: usize,
    style: BlockStyle,
) -> ParsedBlockDescription<'_> {
    let mut base_indent: Option<usize> = None;
    let mut invalid_cause = None;
    let mut line_offset = start;
    let mut block_end = header.len();

    while let Some(line) = header_line_at(header, line_offset) {
        line_offset = line.next;
        if is_structural_blank(line.bytes) {
            continue;
        }
        let indent = leading_space_count(line.bytes);
        if indent == 0 {
            if line.bytes.first() == Some(&b'\t') {
                invalid_cause.get_or_insert(InvalidMetadataCause::UnsupportedMultiline);
                continue;
            }
            block_end = line.start;
            line_offset = line.start;
            break;
        }
        if line.bytes.get(indent) == Some(&b'\t') {
            invalid_cause.get_or_insert(InvalidMetadataCause::UnsupportedMultiline);
            continue;
        }
        let established = *base_indent.get_or_insert(indent);
        if indent < established {
            invalid_cause.get_or_insert(InvalidMetadataCause::UnsupportedMultiline);
            continue;
        }
        if let Some(cause) = invalid_text_cause(&line.bytes[established..]) {
            invalid_cause.get_or_insert(cause);
        }
    }

    ParsedBlockDescription {
        value: &header[start..block_end],
        description: BlockDescription {
            style,
            base_indent: base_indent.unwrap_or(0),
        },
        next_offset: line_offset,
        invalid_cause,
    }
}

fn block_style(value: &[u8]) -> Option<BlockStyle> {
    match value {
        b">" => Some(BlockStyle::FoldedClip),
        b">-" => Some(BlockStyle::FoldedStrip),
        b"|" => Some(BlockStyle::LiteralClip),
        _ => None,
    }
}

fn is_structural_blank(line: &[u8]) -> bool {
    trim(line, BLANK).is_empty()
}

fn leading_space_count(line: &[u8]) -> usize {
    line.iter().take_while(|&&byte| byte == b' ').count()
}

fn decode_block_description(raw: &[u8], block: BlockDescription) -> Vec<u8> {
    let mut last_nonblank_next = 0;
    let mut line_offset = 0;
    while let Some(line) = header_line_at(raw, line_offset) {
        line_offset = line.next;
        if !is_structural_blank(line.bytes) {
            last_nonblank_next = line.next;
        }
    }
    let mut output = Vec::new();
    if last_nonblank_next == 0 {
        return output;
    }

    let mut previous_nonblank = false;
    let mut first = true;
    line_offset = 0;
    while line_offset < last_nonblank_next {
        let Some(line) = header_line_at(raw, line_offset) else {
            break;
        };
        line_offset = line.next;
        let nonblank = !is_structural_blank(line.bytes);
        if !first {
            let folds = block.style != BlockStyle::LiteralClip && previous_nonblank && nonblank;
            output.push(if folds { b' ' } else { b'\n' });
        }
        if nonblank {
            output.extend_from_slice(line.bytes.get(block.base_indent..).unwrap_or_default());
        }
        previous_nonblank = nonblank;
        first = false;
    }
    if block.style != BlockStyle::FoldedStrip {
        output.push(b'\n');
    }
    output
}

pub(super) fn frontmatter_header_start(content: &[u8]) -> Option<usize> {
    if content.starts_with(b"---\r\n") {
        Some(5)
    } else if content.starts_with(b"---\n") {
        Some(4)
    } else if content == b"---" {
        Some(3)
    } else {
        None
    }
}

struct ClosingDelimiter {
    header_end: usize,
    body_start: usize,
}

fn find_closing_delimiter(content: &[u8], header_start: usize) -> Option<ClosingDelimiter> {
    let mut line_start = header_start;
    while line_start <= content.len() {
        let newline = content[line_start..].iter().position(|&byte| byte == b'\n');
        let line_end = newline.map_or(content.len(), |offset| line_start + offset);
        let raw_line = &content[line_start..line_end];
        let line = match newline {
            Some(_) => raw_line.strip_suffix(b"\r").unwrap_or(raw_line),
            None => raw_line,
        };
        if line == b"---" {
            return Some(ClosingDelimiter {
                header_end: line_start,
                body_start: if newline.is_some() {
                    line_end + 1
                } else {
                    line_end
                },
            });
        }
        newline?;
        line_start = line_end + 1;
    }
    None
}

fn parse_recognized_value(value: &[u8]) -> (&[u8], Option<InvalidMetadataCause>) {
    if matches!(value.first(), Some(b'|' | b'>')) {
        return (value, Some(InvalidMetadataCause::UnsupportedMultiline));
    }
    let is_quote = |byte: Option<&u8>| matches!(byte, Some(b'\'' | b'"'));
    if is_quote(value.first()) || is_quote(value.last()) {
        if value.len() >= 2 && value.first() == value.last() {
            return (&value[1..value.len() - 1], None);
        }
        return (value, Some(InvalidMetadataCause::MalformedQuote));
    }
    (value, None)
}

fn invalid_text_cause(value: &[u8]) -> Option<InvalidMetadataCause> {
    if std::str::from_utf8(value).is_err() {
        return Some(InvalidMetadataCause::InvalidUtf8);
    }
    value
        .iter()
        .any(|&byte| byte < 0x20 || byte == 0x7f)
        .then_some(InvalidMetadataCause::ControlByte)
}

pub(crate) fn invalid_skill_name_cause(name: &[u8]) -> Option<InvalidMetadataCause> {
    if name.is_empty() {
        return Some(InvalidMetadataCause::MissingName);
    }
    if name.len() > MAX_NAME_BYTES {
        return Some(InvalidMetadataCause::NameTooLong);
    }
    invalid_text_cause(name)
        .or_else(|| is_path_shaped(name).then_some(InvalidMetadataCause::InvalidName))
}

fn is_path_shaped(name: &[u8]) -> bool {
    name == b"." || name == b".." || name.contains(&b'/') || name.contains(&b'\\')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(content: &[u8]) -> MetadataStatus {
        parse_skill_file(content).status
    }

    fn invalid(cause: InvalidMetadataCause) -> MetadataStatus {
        MetadataStatus::Invalid(cause)
    }

    fn expect_resolved_description(content: &str, expected: &str) {
        let parsed = parse_skill_file(content.as_bytes());
        let metadata = resolve_metadata(&parsed, b"fallback").unwrap();
        assert_eq!(metadata.description, expected);
    }

    #[test]
    fn parse_skill_file_with_full_frontmatter() {
        let parsed = parse_skill_file(
            b"---\nname: my-skill\ndescription: Helps with testing\n---\n\n# My Skill\n\nDo the thing.",
        );
        assert_eq!(parsed.name, Some(&b"my-skill"[..]));
        assert_eq!(parsed.description, Some(&b"Helps with testing"[..]));
        assert_eq!(parsed.body, b"# My Skill\n\nDo the thing.");
    }

    #[test]
    fn parse_skill_file_accepts_supported_description_block_forms() {
        for content in [
            "---\nname: folded\ndescription: >\n  Fold this\n  onto one line.\n---\nBody",
            "---\nname: folded-strip\ndescription: >-\n  Fold without\n  a trailing newline.\n---\nBody",
            "---\nname: literal\ndescription: |\n  Keep this\n  on two lines.\n---\nBody",
        ] {
            assert_eq!(
                status(content.as_bytes()),
                MetadataStatus::Valid,
                "{content}"
            );
        }
    }

    #[test]
    fn description_blocks_preserve_folded_literal_and_trailing_newline_semantics() {
        expect_resolved_description(
            "---\nname: folded\ndescription: >\n  Fold this\n  onto one line.\n\n  Keep this paragraph.\n---\nBody",
            "Fold this onto one line.\n\nKeep this paragraph.\n",
        );
        expect_resolved_description(
            "---\nname: folded-strip\ndescription: >-\n  Fold without\n  a trailing newline.\n---\nBody",
            "Fold without a trailing newline.",
        );
        expect_resolved_description(
            "---\nname: literal\ndescription: |\n  Keep this\n  on two lines.\n---\nBody",
            "Keep this\non two lines.\n",
        );
        expect_resolved_description(
            "---\r\nname: crlf\r\ndescription: |\r\n  first\r\n  second\r\n---\r\nBody",
            "first\nsecond\n",
        );
        expect_resolved_description("---\nname: empty\ndescription: >\n\n---\nBody", "");
    }

    #[test]
    fn description_blocks_return_to_top_level_metadata_and_reject_malformed_structure() {
        let content =
            "---\ndescription: >-\n  first\n    extra indent\nname: after-block\n---\nBody";
        let valid = parse_skill_file(content.as_bytes());
        assert_eq!(valid.status, MetadataStatus::Valid);
        assert_eq!(valid.name, Some(&b"after-block"[..]));
        expect_resolved_description(content, "first   extra indent");

        let cases: [(&[u8], InvalidMetadataCause); 6] = [
            (
                b"---\nname: unsupported\ndescription: >+\n  value\n---\n",
                InvalidMetadataCause::UnsupportedMultiline,
            ),
            (
                b"---\nname: >\n  block names stay invalid\n---\n",
                InvalidMetadataCause::UnsupportedMultiline,
            ),
            (
                b"---\nname: tabbed\ndescription: >\n\tvalue\n---\n",
                InvalidMetadataCause::UnsupportedMultiline,
            ),
            (
                b"---\nname: shallow\ndescription: >\n   first\n  smaller indent\n---\n",
                InvalidMetadataCause::UnsupportedMultiline,
            ),
            (
                b"---\nname: invalid-utf8\ndescription: >\n  bad\xff\n---\n",
                InvalidMetadataCause::InvalidUtf8,
            ),
            (
                b"---\nname: control\ndescription: >\n  bad\x01\n---\n",
                InvalidMetadataCause::ControlByte,
            ),
        ];
        for (content, cause) in cases {
            assert_eq!(status(content), invalid(cause));
        }
    }

    #[test]
    fn skill_metadata_accepts_descriptions_within_the_frontmatter_bound() {
        let description = "use this workflow for the specified task ".repeat(160);
        let content = format!("---\nname: thorough\ndescription: {description}\n---\nbody");
        assert_eq!(status(content.as_bytes()), MetadataStatus::Valid);
    }

    #[test]
    fn description_blocks_respect_the_complete_frontmatter_bound() {
        let prefix = "---\nname: bounded\ndescription: >-\n  ";
        let suffix = "\n---\n";
        let exact = "d".repeat(MAX_FRONTMATTER_BYTES - prefix.len() - suffix.len());
        let over = format!("{exact}d");
        assert_eq!(
            status(format!("{prefix}{exact}{suffix}").as_bytes()),
            MetadataStatus::Valid
        );
        assert_eq!(
            status(format!("{prefix}{over}{suffix}").as_bytes()),
            invalid(InvalidMetadataCause::FrontmatterTooLong)
        );
    }

    #[test]
    fn parse_skill_file_without_frontmatter_returns_full_content_as_body() {
        let content = b"# Just Markdown\n\nSome content.";
        let parsed = parse_skill_file(content);
        assert_eq!(parsed.status, MetadataStatus::NoFrontmatter);
        assert_eq!(parsed.name, None);
        assert_eq!(parsed.description, None);
        assert_eq!(parsed.body, content);
    }

    #[test]
    fn parse_skill_file_with_partial_frontmatter() {
        let parsed = parse_skill_file(b"---\nname: partial\n---\n\nBody here.");
        assert_eq!(parsed.status, MetadataStatus::Valid);
        assert_eq!(parsed.name, Some(&b"partial"[..]));
        assert_eq!(parsed.description, None);
        assert_eq!(parsed.body, b"Body here.");
    }

    #[test]
    fn parse_skill_file_enforces_hard_metadata_field_bounds() {
        let valid_name = "n".repeat(MAX_NAME_BYTES);
        let prefix = "---\nname: valid\ndescription: ";
        let suffix = "\n---\n";
        let valid_description = "d".repeat(MAX_FRONTMATTER_BYTES - prefix.len() - suffix.len());

        assert_eq!(
            status(format!("---\nname: {valid_name}\n---\n").as_bytes()),
            MetadataStatus::Valid
        );
        assert_eq!(
            status(format!("---\nname: {valid_name}n\n---\n").as_bytes()),
            invalid(InvalidMetadataCause::NameTooLong)
        );
        assert_eq!(
            status(format!("{prefix}{valid_description}{suffix}").as_bytes()),
            MetadataStatus::Valid
        );
        assert_eq!(
            status(format!("{prefix}{valid_description}d{suffix}").as_bytes()),
            invalid(InvalidMetadataCause::FrontmatterTooLong)
        );
    }

    #[test]
    fn parse_skill_file_normalizes_crlf_delimiters_and_simply_quoted_values() {
        let parsed = parse_skill_file(
            b"---\r\nname: \"windows-newline\"\r\ndescription: 'quoted description'\r\n---\r\nBody",
        );
        assert_eq!(parsed.status, MetadataStatus::Valid);
        assert_eq!(parsed.name, Some(&b"windows-newline"[..]));
        assert_eq!(parsed.description, Some(&b"quoted description"[..]));
        assert_eq!(parsed.body, b"Body");

        let mixed =
            parse_skill_file(b"---\r\nname: mixed-endings\ndescription: mixed\r\n---\nBody");
        assert_eq!(mixed.status, MetadataStatus::Valid);
        assert_eq!(mixed.name, Some(&b"mixed-endings"[..]));
        assert_eq!(mixed.description, Some(&b"mixed"[..]));
        assert_eq!(mixed.body, b"Body");
    }

    #[test]
    fn parse_skill_file_ignores_unknown_keys_and_colonless_lines() {
        let parsed = parse_skill_file(
            b"---\nignored\nname: known\nextra: value\ndescription: useful\n---\nBody",
        );
        assert_eq!(parsed.name, Some(&b"known"[..]));
        assert_eq!(parsed.description, Some(&b"useful"[..]));
        assert_eq!(parsed.body, b"Body");
    }

    #[test]
    fn parse_skill_file_removes_one_matching_pair_of_outer_quotes() {
        let parsed = parse_skill_file(
            b"---\nname: \"quoted-name\"\ndescription: 'quoted description'\n---\nBody",
        );
        assert_eq!(parsed.status, MetadataStatus::Valid);
        assert_eq!(parsed.name, Some(&b"quoted-name"[..]));
        assert_eq!(parsed.description, Some(&b"quoted description"[..]));
    }

    #[test]
    fn resolve_metadata_gives_discovery_and_installation_one_validity_result() {
        let legacy = resolve_metadata(&parse_skill_file(b"# Legacy\n"), b"legacy").unwrap();
        assert_eq!(legacy.name, "legacy");
        assert_eq!(legacy.description, "");

        let valid = resolve_metadata(
            &parse_skill_file(b"---\nname: review\ndescription: 'review helper'\n---\nbody"),
            b"fallback",
        )
        .unwrap();
        assert_eq!(valid.name, "review");
        assert_eq!(valid.description, "review helper");

        let malformed = resolve_metadata(
            &parse_skill_file(b"---\nname: first\nname: second\n---\nbody"),
            b"fallback",
        );
        assert_eq!(malformed, Err(InvalidMetadataCause::DuplicateRecognizedKey));

        let unsafe_legacy = resolve_metadata(&parse_skill_file(b"# Legacy\n"), b"../unsafe");
        assert_eq!(unsafe_legacy, Err(InvalidMetadataCause::InvalidName));
    }

    #[test]
    fn parse_skill_file_accepts_an_exact_closing_delimiter_at_end_of_file() {
        let parsed = parse_skill_file(b"---\nname: eof-close\n---");
        assert_eq!(parsed.status, MetadataStatus::Valid);
        assert_eq!(parsed.name, Some(&b"eof-close"[..]));
        assert_eq!(parsed.body, b"");
    }

    #[test]
    fn parse_skill_file_classifies_invalid_recognized_metadata() {
        type Case = (&'static [u8], InvalidMetadataCause, Option<&'static [u8]>);
        let cases: [Case; 11] = [
            (
                b"---\nname: unclosed",
                InvalidMetadataCause::MissingClosingDelimiter,
                None,
            ),
            (
                b"---\nname: prefixed-close\n---suffix\nBody",
                InvalidMetadataCause::MissingClosingDelimiter,
                None,
            ),
            (
                b"---\nname: bare-cr-close\n---\r",
                InvalidMetadataCause::MissingClosingDelimiter,
                None,
            ),
            (
                b"---\ndescription: missing name\n---\nBody",
                InvalidMetadataCause::MissingName,
                None,
            ),
            (
                b"---\nname: \"\"\n---\nBody",
                InvalidMetadataCause::MissingName,
                Some(b""),
            ),
            (
                b"---\nname: first\nname: second\n---\nBody",
                InvalidMetadataCause::DuplicateRecognizedKey,
                Some(b"second"),
            ),
            (
                b"---\nname: ../unsafe\n---\nBody",
                InvalidMetadataCause::InvalidName,
                Some(b"../unsafe"),
            ),
            (
                b"---\nname: \"unterminated\n---\nBody",
                InvalidMetadataCause::MalformedQuote,
                Some(b"\"unterminated"),
            ),
            (
                b"---\nname: multiline\ndescription: |2\n---\nBody",
                InvalidMetadataCause::UnsupportedMultiline,
                Some(b"multiline"),
            ),
            (
                b"---\nname: invalid\xff\n---\nBody",
                InvalidMetadataCause::InvalidUtf8,
                Some(b"invalid\xff"),
            ),
            (
                b"---\nname: control\x01byte\n---\nBody",
                InvalidMetadataCause::ControlByte,
                Some(b"control\x01byte"),
            ),
        ];
        for (content, cause, expected_name) in cases {
            let parsed = parse_skill_file(content);
            assert_eq!(parsed.status, invalid(cause));
            assert_eq!(parsed.name, expected_name);
        }
    }

    #[test]
    fn parse_skill_file_rejects_indented_continuations_for_recognized_metadata() {
        for content in [
            "---\nname: workflow\n  continued name\ndescription: helper\n---\nBody",
            "---\nname: workflow\ndescription: first line\n  continued description\n---\nBody",
            "---\nname: workflow\ndescription: first line\n  continued: description\n---\nBody",
            "---\nname: workflow\ndescription:\n  continued description\n---\nBody",
        ] {
            assert_eq!(
                status(content.as_bytes()),
                invalid(InvalidMetadataCause::UnsupportedMultiline),
                "{content}"
            );
        }
    }

    #[test]
    fn parse_skill_file_keeps_fuzzed_borrowed_slices_inside_the_input() {
        let corpus: [&[u8]; 6] = [
            b"",
            b"plain body",
            b"---\nname: valid\n---\nbody",
            b"---\r\nname: \"valid\"\r\n---\r\nbody",
            b"---\nname: quoted\ndescription: 'single line'\n---\nbody",
            b"---\nname: block\ndescription: >-\n  first line\n  second line\n---\nbody",
        ];
        for input in corpus {
            for end in 0..=input.len() {
                let prefix = &input[..end];
                let parsed = parse_skill_file(prefix);
                let range = prefix.as_ptr_range();
                for borrowed in [Some(parsed.body), parsed.name, parsed.description]
                    .into_iter()
                    .flatten()
                {
                    let borrowed_range = borrowed.as_ptr_range();
                    assert!(borrowed_range.start >= range.start);
                    assert!(borrowed_range.end <= range.end);
                }
                if let Ok(metadata) = resolve_metadata(&parsed, b"fallback") {
                    assert!(metadata.description.len() <= prefix.len());
                }
            }
        }
    }

    #[test]
    fn validate_managed_skill_name_accepts_plain_names_and_rejects_path_shapes() {
        assert_eq!(invalid_skill_name_cause(b"review"), None);
        assert_eq!(
            invalid_skill_name_cause(b""),
            Some(InvalidMetadataCause::MissingName)
        );
        for name in [".", "..", "nested/name", "nested\\name", "/absolute"] {
            assert_eq!(
                invalid_skill_name_cause(name.as_bytes()),
                Some(InvalidMetadataCause::InvalidName),
                "{name}"
            );
        }
    }
}
