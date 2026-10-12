use super::*;
use crate::session_manager::frame::MAX_LINE_BYTES;
use crate::session_manager::log::check_line;
use crate::session_manager::schema::parse_line_body;
use crate::session_manager::test_support::fixture_log;

fn fixture_lines(scenario: &str, id: &str) -> Vec<Vec<u8>> {
    fixture_log(scenario, id)
        .split_inclusive(|&byte| byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect()
}

fn line_at(lines: &[Vec<u8>], seq: u64) -> &[u8] {
    lines
        .iter()
        .find(|line| check_line(line).unwrap().seq == seq)
        .unwrap()
}

fn written(seq: u64, ts_ms: u64, line: &Line<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    append_line(&mut out, seq, ts_ms, line).unwrap();
    out
}

#[test]
fn json_strings_escape_only_what_upstream_escapes() {
    let cases = [
        ("plain", "\"plain\""),
        ("q\"b\\", "\"q\\\"b\\\\\""),
        ("\n\r\t", "\"\\n\\r\\t\""),
        ("\u{0}\u{8}\u{c}\u{1f}", "\"\\u0000\\u0008\\u000c\\u001f\""),
        ("/\u{7f}é Проверь", "\"/\u{7f}é Проверь\""),
    ];
    for (text, expected) in cases {
        let mut out = b"kept".to_vec();
        append_json_string(&mut out, text);
        assert_eq!(&out[4..], expected.as_bytes(), "{text:?}");
    }
}

#[test]
fn written_lines_equal_the_lines_fx_wrote_byte_for_byte() {
    let tool = fixture_lines("tool", "ia8mVXaiyAf1");
    let mut created = Vec::new();
    append_created(
        &mut created,
        1_791_772_892_509,
        &Identity {
            id: "ia8mVXaiyAf1".to_owned(),
            workspace: "/private/tmp/fxv2/tool/workspace".to_owned(),
            role: Role::Root,
            host: Host::Ask,
        },
    )
    .unwrap();
    assert_eq!(created, line_at(&tool, 1));
    for seq in [2, 3, 4, 5, 6, 15, 16, 17, 20, 21, 22, 23, 25] {
        let expected = line_at(&tool, seq);
        let header = check_line(expected).unwrap();
        let body = parse_line_body(header.kind.unwrap(), expected).unwrap();
        let line = match &body {
            Body::Set { key, value, .. } => Line::Set(*key, value),
            Body::TurnStarted { turn } => Line::TurnStarted(*turn),
            Body::Item {
                item_type,
                data,
                blobs,
            } => Line::Item {
                turn: 1,
                item_type,
                data,
                blobs,
            },
            Body::TurnCommitted => Line::TurnCommitted(1),
            Body::Closed => Line::Closed,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(written(seq, header.ts_ms, &line), expected, "seq {seq}");
    }
    let killed = fixture_lines("kill_tool", "NYXx_508J28b");
    assert_eq!(
        written(
            36,
            1_791_772_893_044,
            &Line::TurnInterrupted(2, Reason::Crash)
        ),
        line_at(&killed, 36)
    );
}

#[test]
fn every_written_line_checks_and_reads_back_as_its_body() {
    let blobs = vec!["a".repeat(64), "b".repeat(64)];
    let lines = [
        Line::TurnStarted(7),
        Line::Item {
            turn: 7,
            item_type: "tool_result",
            data: b"{\"text\":\"line\\nbreak \\u00e9\"}",
            blobs: &blobs,
        },
        Line::Item {
            turn: 7,
            item_type: "user",
            data: b"[1,2]",
            blobs: &[],
        },
        Line::TurnCommitted(7),
        Line::TurnInterrupted(8, Reason::Cancel),
        Line::Set(SetKey::Title, b"\"t\\\"itle\""),
        Line::Closed,
    ];
    for (seq, line) in (1..).zip(&lines) {
        let bytes = written(seq, 42, line);
        let header = check_line(&bytes).unwrap();
        assert_eq!((header.seq, header.ts_ms), (seq, 42));
        assert_eq!(header.kind, Some(line.kind()));
        assert_eq!(
            parse_line_body(line.kind(), &bytes).unwrap(),
            line.body(),
            "{line:?}"
        );
    }
    let mut out = Vec::new();
    append_line(
        &mut out,
        1,
        1,
        &Line::Item {
            turn: 1,
            item_type: "user",
            data: b"{}",
            blobs: &[],
        },
    )
    .unwrap();
    assert!(!out.windows(8).any(|window| window == b"\"blobs\":"));
}

#[test]
fn a_line_over_the_cap_is_refused_and_the_buffer_kept() {
    let data = format!("\"{}\"", "x".repeat(MAX_LINE_BYTES));
    let mut out = b"kept".to_vec();
    assert_eq!(
        append_line(&mut out, 1, 1, &Line::Set(SetKey::Title, data.as_bytes())),
        Err(TooLarge)
    );
    assert_eq!(out, b"kept");
}
