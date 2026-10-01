use ofx_config::parse_strict_json;

use super::*;

const DOCUMENTS: [&str; 28] = [
    r#"{"id":"chat-1","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
    r#"{"b":1,"a":[true,false,null,"x",1.5,-2,0,-0,0.0,-0.0]}"#,
    r#" { "spaced" : [ 1 , 2 ] } "#,
    r#"{"escapes":"\"\\\/\b\f\n\r\t\u0000\u001f\u007f\u00e9\ud83d\ude00","raw":"é😀"}"#,
    r#"{"numbers":[18446744073709551615,18446744073709551616,-9223372036854775808,-9223372036854775809]}"#,
    r#"{"floats":[1e5,1E-7,2.5e+3,123456789012345678901234567890,0.1,1.7976931348623157e308]}"#,
    r#"{"nested":{"deeper":{"deepest":[[[{}]],[]]}}}"#,
    r#"{"a":1,"a":2}"#,
    r#"{"a":1,"\u0061":2}"#,
    r#"{"outer":{"x":1,"x":2}}"#,
    r#"[{"x":1},{"x":1,"x":1}]"#,
    r#"{"k0":0,"k1":1,"k2":2,"k3":3,"k4":4,"k5":5,"k6":6,"k7":7,"k8":8,"k9":9,"k10":10,"k11":11,"k12":12,"k13":13,"k14":14,"k15":15,"k16":16,"k17":17}"#,
    r#"{"k0":0,"k1":1,"k2":2,"k3":3,"k4":4,"k5":5,"k6":6,"k7":7,"k8":8,"k9":9,"k10":10,"k11":11,"k12":12,"k13":13,"k14":14,"k15":15,"k16":16,"k0":17}"#,
    r#"{"lone":"\ud800"}"#,
    r#"{"bad escape":"\x"}"#,
    "{\"control\":\"a\u{1}b\"}",
    r#"{"overflow":1e400}"#,
    r#"{"trailing":1,}"#,
    r#"{"unterminated":"#,
    "{} trailing",
    r#"{"leading zero":01}"#,
    "[1,2,]",
    r#""text""#,
    "null",
    "12",
    "",
    r#"{"empty key":{"":1,"":2}}"#,
    r#"{"same prefix":{"ab":1,"a":2,"abc":3}}"#,
];

const INVALID_UTF8: [&[u8]; 2] = [b"{\"bytes\":\"\xff\"}", b"{\"\xc3\x28\":1}"];

#[test]
fn parsing_accepts_and_rejects_exactly_what_the_strict_value_parser_does() {
    let documents = DOCUMENTS.iter().map(|text| text.as_bytes());
    for document in documents.chain(INVALID_UTF8) {
        let borrowed = parse(document).map(|json| compact(&json));
        let strict = parse_strict_json(document)
            .ok()
            .map(|value| value.to_string());
        assert_eq!(borrowed, strict, "{}", String::from_utf8_lossy(document));
    }
}

#[test]
fn large_objects_still_reject_duplicate_keys() {
    let unique: Vec<String> = (0..1000)
        .map(|index| format!("\"k{index}\":{index}"))
        .collect();
    let mut repeated = unique.clone();
    repeated.push("\"k500\":0".to_owned());
    assert!(parse(format!("{{{}}}", unique.join(",")).as_bytes()).is_some());
    assert!(parse(format!("{{{}}}", repeated.join(",")).as_bytes()).is_none());
}

#[test]
fn strings_borrow_the_input_unless_they_contain_escapes() {
    let Some(Json::Object(object)) = parse(br#"{"plain":"text","escaped":"a\nb","k\u0065y":1}"#)
    else {
        panic!("an object");
    };
    assert!(matches!(
        object.get("plain"),
        Some(Json::String(Cow::Borrowed("text")))
    ));
    assert!(
        matches!(object.get("escaped"), Some(Json::String(Cow::Owned(text))) if text == "a\nb")
    );
    assert_eq!(object.get("key").and_then(Json::as_i64), Some(1));
    assert_eq!(
        object.iter().map(|(key, _)| key).collect::<Vec<_>>(),
        ["plain", "escaped", "key"]
    );
}

#[test]
fn integers_read_back_only_when_they_fit_a_signed_64_bit_value() {
    let Some(Json::Array(items)) =
        parse(br#"[0,-1,9223372036854775807,9223372036854775808,1.0,"1",null]"#)
    else {
        panic!("an array");
    };
    let read: Vec<Option<i64>> = items.iter().map(Json::as_i64).collect();
    assert_eq!(
        read,
        [Some(0), Some(-1), Some(i64::MAX), None, None, None, None]
    );
    assert!(items[6].is_null());
    assert_eq!(items[5].as_str(), Some("1"));
    assert!(items[0].as_array().is_none() && items[0].as_object().is_none());
}
