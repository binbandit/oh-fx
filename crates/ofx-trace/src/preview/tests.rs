use super::*;

#[test]
fn preview_trims_and_keeps_the_first_line() {
    assert_eq!(preview(" \t\nfirst\nsecond\n ", 128), "first");
}

#[test]
fn preview_trims_a_trailing_carriage_return_for_crlf_input() {
    assert_eq!(preview("first\r\nsecond", 128), "first");
}

#[test]
fn preview_clamps_by_byte_length() {
    assert_eq!(preview("abcdef", 4), "abcd");
}

#[test]
fn preview_returns_a_borrowed_slice() {
    let input = "  borrowed  ";
    let result = preview(input, 128);
    let start = input.as_ptr() as usize;
    let at = result.as_ptr() as usize;
    assert!(at >= start && at < start + input.len());
}

#[test]
fn preview_cuts_on_a_character_boundary() {
    assert_eq!(preview("aé", 2), "a");
}

#[test]
fn terminal_preview_strips_common_terminal_controls() {
    assert_eq!(
        terminal_preview(
            "hi \x1b[31mred\x1b[0m \x1b]8;;https://example.com\x07link\x1b]8;;\x07\nnext",
            64
        ),
        "hi red link"
    );
}

#[test]
fn terminal_preview_replaces_control_bytes_and_stops_at_its_room() {
    assert_eq!(terminal_preview("a\tb\rc\x01d\x7fe", 64), "a b c?d?e");
    assert_eq!(terminal_preview("abcdef", 4), "abcd");
    assert_eq!(terminal_preview("aé", 2), "a");
    assert_eq!(terminal_preview("x\x1b]0;title\x1b\\y\x1bZz", 64), "xyz");
    assert_eq!(terminal_preview("", 0), "");
}
