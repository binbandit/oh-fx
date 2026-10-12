use super::*;
use crate::session_manager::log::check_line;
use crate::session_manager::test_support::{fixture_log, line};

const LOGS: [(&str, &str); 7] = [
    ("listing", "nJI52ukeF2bU"),
    ("listing", "U9K-4ZzltsUh"),
    ("listing", "pgdu6JEc1UOO"),
    ("listing", "hDoX4WdPFWDK"),
    ("tool", "ia8mVXaiyAf1"),
    ("kill_tool", "NYXx_508J28b"),
    ("snapshot_small", "PsjMeU3Vl5Ld"),
];

#[test]
fn every_line_fx_wrote_checks_and_its_body_parses() {
    let mut seen = Vec::new();
    for (scenario, id) in LOGS {
        let log = fixture_log(scenario, id);
        for (index, bytes) in log.split_inclusive(|&byte| byte == b'\n').enumerate() {
            let header = check_line(bytes).unwrap();
            assert_eq!(header.seq, index as u64 + 1);
            let kind = header.kind.unwrap();
            parse_line_body(kind, bytes).unwrap();
            if !seen.contains(&kind) {
                seen.push(kind);
            }
        }
    }
    for kind in [
        Kind::SessionCreated,
        Kind::TurnStarted,
        Kind::Item,
        Kind::TurnCommitted,
        Kind::TurnInterrupted,
        Kind::Set,
        Kind::Snapshot,
        Kind::Closed,
    ] {
        assert!(seen.contains(&kind), "{kind:?}");
    }
}

#[test]
fn unknown_kinds_parse_with_no_kind() {
    let header = check_line(&line(3, "future_kind", ",\"x\":1")).unwrap();
    assert_eq!(header.seq, 3);
    assert_eq!(header.kind, None);
}

#[test]
fn malformed_headers_are_rejected() {
    for header in [
        "{\"seq\":1,\"v\":1,\"ts\":1,\"kind\":\"closed\"",
        "{\"v\":0,\"seq\":1,\"ts\":1,\"kind\":\"closed\"",
        "{\"v\":1,\"seq\":0,\"ts\":1,\"kind\":\"closed\"",
        "{\"v\":01,\"seq\":1,\"ts\":1,\"kind\":\"closed\"",
        "{\"v\":1,\"seq\":-1,\"ts\":1,\"kind\":\"closed\"",
        "{\"v\":1,\"seq\":1,\"ts\":1,\"kind\":\"\"",
        "{\"v\":1,\"seq\":1,\"ts\":1,\"kind\":\"Closed\"",
        "{\"v\":1,\"seq\":1,\"ts\":1,\"kind\":\"closed",
        "{\"v\":1,\"seq\":18446744073709551616,\"ts\":1,\"kind\":\"closed\"",
        "{\"v\":4294967296,\"seq\":1,\"ts\":1,\"kind\":\"closed\"",
        "{\"v\":1,\"seq\":1,\"ts\":1,\"kind\":\"abcdefghijklmnopqrstuvwxyzabcdefg\"",
        " {\"v\":1,\"seq\":1,\"ts\":1,\"kind\":\"closed\"",
        "{\"v\":1, \"seq\":1,\"ts\":1,\"kind\":\"closed\"",
        "{\"v\":1,\"seq\":1,\"ts\":1,\"kind\":\"closed\"x",
    ] {
        assert_eq!(parse_header(header.as_bytes()), None, "{header}");
    }
    assert_eq!(
        parse_header(b"{\"v\":1,\"seq\":7,\"ts\":0,\"kind\":\"closed\""),
        Some(Header {
            seq: 7,
            kind: Some(Kind::Closed)
        })
    );
}

#[test]
fn header_parsing_never_panics_on_arbitrary_bytes() {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let header = b"{\"v\":1,\"seq\":1,\"ts\":1,\"kind\":\"closed\"";
    for length in 0..header.len() {
        parse_header(&header[..length]);
    }
    for _ in 0..2000 {
        let mut bytes = header.to_vec();
        for _ in 0..3 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let at = usize::try_from(state >> 33).unwrap() % bytes.len();
            bytes[at] = u8::try_from(state >> 56).unwrap();
        }
        parse_header(&bytes);
    }
}

#[test]
fn a_body_missing_a_required_field_is_rejected() {
    for (kind, fields) in [
        (Kind::TurnStarted, ""),
        (Kind::Item, ",\"turn\":1,\"data\":{}"),
        (Kind::Item, ",\"turn\":1,\"type\":\"user\""),
        (Kind::TurnInterrupted, ",\"turn\":1"),
        (Kind::Set, ",\"key\":\"title\""),
        (Kind::Set, ",\"key\":\"color\",\"value\":1"),
        (Kind::ChildFinished, ",\"child\":\"c\",\"work_id\":\"w\""),
        (Kind::Snapshot, ",\"state\":{}"),
        (
            Kind::SessionCreated,
            ",\"id\":\"a\",\"workspace\":\"/w\",\"role\":\"root\"",
        ),
    ] {
        let bytes = line(1, kind.name(), fields);
        assert_eq!(parse_line_body(kind, &bytes), Err(BadBody), "{fields}");
    }
}

#[test]
fn bodies_keep_raw_values_byte_for_byte() {
    let bytes = line(
        2,
        "set",
        ",\"key\":\"title\",\"value\":\"T\\u00e9 \\\"x\\\"\",\"blobs\":[\"aa\",[98]]",
    );
    assert_eq!(
        parse_line_body(Kind::Set, &bytes),
        Ok(Body::Set {
            key: SetKey::Title,
            value: br#""T\u00e9 \"x\"""#,
            blobs: vec!["aa".to_owned(), "b".to_owned()],
        })
    );
    let bytes = line(
        1,
        "session_created",
        ",\"id\":\"f\",\"workspace\":\"/w\",\"role\":\"root\",\"host\":\"ask\",\"forked_from\":{\"id\":\"s\",\"seq\":9}",
    );
    assert_eq!(
        parse_line_body(Kind::SessionCreated, &bytes),
        Ok(Body::SessionCreated { fork_seq: 9 })
    );
    for origin in [
        "{\"id\":\"s\"}",
        "{\"id\":\"s\",\"seq\":9,\"x\":1}",
        "{\"id\":\"s\",\"seq\":9,\"seq\":9}",
    ] {
        let bytes = line(
            1,
            "session_created",
            &format!(
                ",\"id\":\"f\",\"workspace\":\"/w\",\"role\":\"root\",\"host\":\"ask\",\"forked_from\":{origin}"
            ),
        );
        assert_eq!(
            parse_line_body(Kind::SessionCreated, &bytes),
            Err(BadBody),
            "{origin}"
        );
    }
}

#[test]
fn ids_follow_v1s_alphabet_and_avoid_the_reserved_names() {
    for id in ["nJI52ukeF2bU", "a", "a.b", "a_b-c", &"x".repeat(255)] {
        assert!(valid_id(id), "{id}");
    }
    for id in [
        "",
        ".hidden",
        "index.jsonl",
        "index.lock",
        "a/b",
        "a b",
        &"x".repeat(256),
    ] {
        assert!(!valid_id(id), "{id}");
    }
    assert!(valid_blob_hash(&"0a".repeat(32)));
    assert!(!valid_blob_hash(&"0A".repeat(32)));
    assert!(!valid_blob_hash(&"0a".repeat(31)));
}
