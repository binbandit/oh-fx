use std::io::Write as _;
use std::os::unix::net::UnixStream;

use super::*;

fn teams() -> Vec<Team> {
    [
        ("team_1", "vercel-labs", "Vercel Labs"),
        ("team_2", "personal", "personal"),
        ("team_3", "odd", "Odd\u{1b}[2J"),
    ]
    .into_iter()
    .map(|(id, slug, name)| Team {
        id: id.to_owned(),
        slug: slug.to_owned(),
        name: name.to_owned(),
    })
    .collect()
}

fn line_choice(typed: &[u8], default_index: usize) -> (Result<usize, VercelError>, String) {
    let (input, mut writer) = UnixStream::pair().unwrap();
    writer.write_all(typed).unwrap();
    drop(writer);
    let mut output = Vec::new();
    let chosen = select_by_line(&teams(), default_index, &input, &mut output);
    (chosen, String::from_utf8(output).unwrap())
}

#[test]
fn the_line_picker_lists_teams_and_reads_one_number_per_prompt() {
    let (chosen, output) = line_choice(b"2\n", 0);
    assert_eq!(chosen, Ok(1));
    let escaped = encode_terminal_safe(b"Odd\x1b[2J", usize::MAX).text;
    assert!(!escaped.contains('\u{1b}'));
    assert_eq!(
        output,
        format!(
            "\nSelect a Vercel team for AI Gateway:\nModel requests will use the selected team; Gateway access may still require billing or API setup.\n\n  1. Vercel Labs (vercel-labs) (default)\n  2. personal (personal)\n  3. {escaped} (odd)\nTeam [1]: "
        )
    );
}

#[test]
fn the_line_picker_takes_the_default_on_an_empty_line_or_end_of_input() {
    assert_eq!(line_choice(b"\n", 2).0, Ok(2));
    assert_eq!(line_choice(b"", 1).0, Ok(1));
    assert_eq!(line_choice(b"  \t\r\n", 1).0, Ok(1));
}

#[test]
fn the_line_picker_asks_again_after_an_invalid_choice() {
    let (chosen, output) = line_choice(b"9\n+1\nx\xff\n3\n", 0);
    assert_eq!(chosen, Ok(2));
    assert_eq!(output.matches(INVALID_CHOICE).count(), 3);
    assert_eq!(output.matches("Team [1]: ").count(), 4);
}

#[test]
fn team_numbers_parse_as_upstreams_unsigned_parser_does() {
    for (choice, expected) in [
        ("3", Some(3)),
        ("0_3", Some(3)),
        ("1__0", Some(10)),
        ("_1", None),
        ("1_", None),
        ("+1", None),
        ("-1", None),
        ("1 2", None),
        ("99999999999999999999999", None),
    ] {
        assert_eq!(parse_choice(choice), expected, "{choice}");
    }
}

#[test]
fn the_interactive_picker_marks_the_selection_and_redraws_in_place() {
    let mut first = Vec::new();
    render(&teams()[..2], 0, 1, true, &mut first).unwrap();
    assert_eq!(
        String::from_utf8(first).unwrap(),
        "\x1b[2K  \u{203a} 1. Vercel Labs (vercel-labs)\n\x1b[2K    2. personal (personal) (current)\n"
    );
    let mut again = Vec::new();
    render(&teams()[..2], 1, 1, false, &mut again).unwrap();
    assert!(
        String::from_utf8(again)
            .unwrap()
            .starts_with("\x1b[2A\r\x1b[2K    1.")
    );
}

#[test]
fn picker_keys_follow_upstream_bindings() {
    for (bytes, expected) in [
        (&b"\r"[..], Key::Enter),
        (b"\n", Key::Enter),
        (b"\x03", Key::Cancel),
        (b"\x04", Key::Cancel),
        (b"3", Key::Number(3)),
        (b"0", Key::Ignored),
        (b"q", Key::Ignored),
        (b"\x1b[A", Key::Up),
        (b"\x1bOB", Key::Down),
        (b"\x1b[C", Key::Ignored),
        (b"\x1b", Key::Cancel),
        (b"\x1bx", Key::Cancel),
        (b"", Key::Ignored),
    ] {
        assert_eq!(parse_key(bytes), expected, "{bytes:?}");
    }
    assert!(!escape_sequence_complete(b"\x1b["));
    assert!(escape_sequence_complete(b"\x1b[A"));
    assert!(escape_sequence_complete(b"\x1bxy"));
    assert!(!escape_sequence_complete(b"\x1b[1"));
    assert!(escape_sequence_complete(b"\x1b[1~"));
}

#[test]
fn picker_keys_collect_a_split_escape_sequence() {
    let (input, mut writer) = UnixStream::pair().unwrap();
    writer.write_all(b"\x1b[B").unwrap();
    assert_eq!(read_key(&input), Ok(Key::Down));
    writer.write_all(b"\x1b").unwrap();
    assert_eq!(read_key(&input), Ok(Key::Cancel));
    drop(writer);
    assert_eq!(read_key(&input), Ok(Key::Ignored));
}
