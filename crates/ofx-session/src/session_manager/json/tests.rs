use super::*;

#[test]
fn values_parse_as_one_json_document_with_surrounding_whitespace() {
    assert_eq!(parse(b"null"), Some(Value::Null));
    assert_eq!(parse(b" true\n"), Some(Value::Bool(true)));
    assert_eq!(parse(b"\t-12.5e+3 "), Some(Value::Number("-12.5e+3")));
    assert_eq!(
        parse(br#""a\"\\\/\b\f\n\r\t\u00e9\ud83d\ude00""#),
        Some(Value::String(
            "a\"\\/\u{8}\u{c}\n\r\t\u{e9}\u{1f600}".to_owned()
        ))
    );
    assert_eq!(
        parse(b"[1, \"x\" ,[2],{\"k\":[]}]"),
        Some(Value::Array(vec![
            &b"1"[..],
            &b"\"x\""[..],
            &b"[2]"[..],
            &b"{\"k\":[]}"[..]
        ]))
    );
    assert_eq!(parse(b"{ \"a\" : 1 , \"a\": 2 }"), Some(Value::Object));
    assert_eq!(parse(b"[]"), Some(Value::Array(Vec::new())));
}

#[test]
fn anything_that_is_not_strict_json_is_refused() {
    for bad in [
        &b""[..],
        b"01",
        b"1.",
        b".5",
        b"1e",
        b"+1",
        b"tru",
        b"nul",
        b"[1,]",
        b"{\"a\":1,}",
        b"{\"a\" 1}",
        b"{a:1}",
        b"[1 2]",
        b"\"\x01\"",
        b"\"\\x\"",
        b"\"\\ud800\"",
        b"\"\\udc00\"",
        b"\"\\ud800\\u0041\"",
        b"\"\xff\"",
        b"1 2",
        b"[",
        b"{\"a\":}",
    ] {
        assert_eq!(parse(bad), None, "{}", String::from_utf8_lossy(bad));
    }
}

#[test]
fn integers_coerce_as_std_json_does() {
    let cases: [(&str, Option<u64>); 22] = [
        ("0", Some(0)),
        ("18446744073709551615", Some(u64::MAX)),
        ("18446744073709551616", None),
        ("-1", None),
        ("-0", Some(0)),
        ("-00", Some(0)),
        ("+7", Some(7)),
        ("1_000", Some(1000)),
        ("_1", None),
        ("1_", None),
        ("1e3", Some(1000)),
        ("2.0", Some(2)),
        ("2.5", None),
        ("25e-1", None),
        ("250e-2", None),
        ("2500e-3", None),
        ("2000e-3", Some(2)),
        ("0.0e5", Some(0)),
        ("-0.0", Some(0)),
        ("1.8446744073709551615e19", Some(u64::MAX)),
        ("1e400", None),
        ("0x10", None),
    ];
    for (text, expected) in cases {
        assert_eq!(slice_to_u64(text), expected, "{text}");
    }
}
