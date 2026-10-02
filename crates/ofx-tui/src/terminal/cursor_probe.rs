use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CursorPosition {
    pub(crate) row: u16,
    pub(crate) col: u16,
}

fn count_digits(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count()
}

pub(crate) fn find_position_response(bytes: &[u8]) -> Option<CursorPosition> {
    find_position_span(bytes).map(|(_, position)| position)
}

pub(crate) fn find_position_span(bytes: &[u8]) -> Option<(Range<usize>, CursorPosition)> {
    (0..bytes.len()).find_map(|start| {
        position_response_at(bytes, start).map(|(end, position)| (start..end, position))
    })
}

fn position_response_at(bytes: &[u8], start: usize) -> Option<(usize, CursorPosition)> {
    let rest = bytes.get(start..)?.strip_prefix(b"\x1b[")?;
    let (row, rest) = split_digits(rest)?;
    let rest = rest.strip_prefix(b";")?;
    let (col, rest) = split_digits(rest)?;
    let rest = rest.strip_prefix(b"R")?;
    let row = parse_coordinate(row)?;
    let col = parse_coordinate(col)?;
    Some((bytes.len() - rest.len(), CursorPosition { row, col }))
}

fn split_digits(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let digits = count_digits(bytes);
    if digits == 0 {
        return None;
    }
    Some(bytes.split_at(digits))
}

fn parse_coordinate(digits: &[u8]) -> Option<u16> {
    let value: u16 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    (value != 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_shot_cursor_response_parser_finds_a_response_with_leading_and_trailing_bytes() {
        assert_eq!(
            find_position_response(b"noise\x1b[42;7Rtail"),
            Some(CursorPosition { row: 42, col: 7 })
        );
    }

    #[test]
    fn position_spans_cover_only_the_reply_bytes() {
        assert_eq!(
            find_position_span(b"ab\x1b[7;3Rc"),
            Some((2..8, CursorPosition { row: 7, col: 3 }))
        );
        assert_eq!(find_position_span(b"\x1b[7;3"), None);
    }

    #[test]
    fn one_shot_cursor_response_parser_rejects_invalid_rows_and_columns() {
        let invalid: [&[u8]; 9] = [
            b"",
            b"\x1b[0;1R",
            b"\x1b[1;0R",
            b"\x1b[;1R",
            b"\x1b[1;R",
            b"\x1b[1;1",
            b"\x1b[999999;1R",
            b"\x1b[1;999999R",
            b"\x9b1;1R",
        ];
        for sample in invalid {
            assert_eq!(find_position_response(sample), None);
        }
    }
}
