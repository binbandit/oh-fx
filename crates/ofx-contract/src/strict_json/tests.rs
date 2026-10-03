use std::borrow::Cow;

use serde_json::Value;

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

fn both_orders() -> [DuplicateKeys; 3] {
    [
        DuplicateKeys::BeforeValue,
        DuplicateKeys::AfterValue,
        DuplicateKeys::AfterObject,
    ]
}

fn compact(json: &Json<'_>) -> String {
    serde_json::to_string(json).unwrap()
}

fn has_duplicate_key(value: &[u8]) -> bool {
    let text = String::from_utf8_lossy(value);
    [
        "\"a\":1,\"a\"",
        "\"a\":1,\"\\u0061\"",
        "\"x\":1,\"x\"",
        "\"x\":1,\"x\":1",
        "\"k0\":17",
        "\"\":1,\"\"",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
}

#[test]
fn parsing_accepts_what_serde_json_accepts_unless_a_key_repeats() {
    let documents = DOCUMENTS.iter().map(|text| text.as_bytes());
    for document in documents.chain(INVALID_UTF8) {
        let plain = serde_json::from_slice::<Value>(document).ok();
        let expected = plain
            .filter(|_| !has_duplicate_key(document))
            .map(|value| value.to_string());
        for duplicates in both_orders() {
            let parsed = parse_strict_json(document, duplicates).ok();
            assert_eq!(
                parsed.as_ref().map(compact),
                expected,
                "{}",
                String::from_utf8_lossy(document)
            );
        }
        assert_eq!(
            parse_strict_json_value(document)
                .ok()
                .map(|value| value.to_string()),
            expected,
            "{}",
            String::from_utf8_lossy(document)
        );
    }
}

#[test]
fn repeated_keys_fail_at_any_depth_after_unescaping() {
    for json in [
        r#"{"a":1,"a":2}"#,
        r#"{"a":1,"\u0061":2}"#,
        r#"{"outer":{"a":1,"a":2}}"#,
        r#"[{"a":1,"a":2}]"#,
    ] {
        for duplicates in both_orders() {
            assert_eq!(
                parse_strict_json(json.as_bytes(), duplicates),
                Err(StrictJsonError::DuplicateField),
                "{json}"
            );
        }
    }
}

#[test]
fn a_repeated_key_fails_before_or_after_its_value_as_asked() {
    for json in [r#"{"a":1,"a":"#, r#"{"a":1,"a":[1,}"#, r#"{"a":1,"a":2"#] {
        assert_eq!(
            parse_strict_json(json.as_bytes(), DuplicateKeys::BeforeValue),
            Err(StrictJsonError::DuplicateField),
            "{json}"
        );
    }
    for json in [r#"{"a":1,"a":"#, r#"{"a":1,"a":[1,}"#] {
        assert_eq!(
            parse_strict_json(json.as_bytes(), DuplicateKeys::AfterValue),
            Err(StrictJsonError::Syntax),
            "{json}"
        );
    }
    assert_eq!(
        parse_strict_json(br#"{"a":1,"a":2"#, DuplicateKeys::AfterValue),
        Err(StrictJsonError::DuplicateField)
    );
    for json in [r#"{"a":1,"a":2"#, r#"{"a":1,"a":2,"#] {
        assert_eq!(
            parse_strict_json(json.as_bytes(), DuplicateKeys::AfterObject),
            Err(StrictJsonError::Syntax),
            "{json}"
        );
    }
    for json in [r#"[{"a":1,"a":2}"#, r#"[{"a":1,"a":2},"#] {
        assert_eq!(
            parse_strict_json(json.as_bytes(), DuplicateKeys::AfterObject),
            Err(StrictJsonError::DuplicateField),
            "{json}"
        );
    }
}

#[test]
fn large_objects_still_reject_repeated_keys() {
    let unique: Vec<String> = (0..1000)
        .map(|index| format!("\"k{index}\":{index}"))
        .collect();
    for repeated_key in ["k0", "k15", "k16", "k500", "k999"] {
        let mut repeated = unique.clone();
        repeated.push(format!("\"{repeated_key}\":0"));
        for duplicates in both_orders() {
            assert!(
                parse_strict_json(format!("{{{}}}", unique.join(",")).as_bytes(), duplicates)
                    .is_ok()
            );
            assert_eq!(
                parse_strict_json(format!("{{{}}}", repeated.join(",")).as_bytes(), duplicates),
                Err(StrictJsonError::DuplicateField),
                "{repeated_key}"
            );
        }
    }
}

#[test]
fn strings_borrow_the_input_unless_they_contain_escapes() {
    let Ok(Json::Object(object)) = parse_strict_json(
        br#"{"plain":"text","escaped":"a\nb","k\u0065y":1}"#,
        DuplicateKeys::BeforeValue,
    ) else {
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
    assert_eq!(object.len(), 3);
    assert_eq!(object.entries()[1].0, "escaped");
}

#[test]
fn integers_read_back_only_when_they_fit() {
    let Ok(Json::Array(items)) = parse_strict_json(
        br#"[0,-1,9223372036854775807,9223372036854775808,18446744073709551615,1.0,"1",null,true]"#,
        DuplicateKeys::BeforeValue,
    ) else {
        panic!("an array");
    };
    let signed: Vec<Option<i64>> = items.iter().map(Json::as_i64).collect();
    assert_eq!(
        signed,
        [
            Some(0),
            Some(-1),
            Some(i64::MAX),
            None,
            None,
            None,
            None,
            None,
            None
        ]
    );
    let unsigned: Vec<Option<u64>> = items.iter().map(Json::as_u64).collect();
    assert_eq!(
        unsigned,
        [
            Some(0),
            None,
            Some(9_223_372_036_854_775_807),
            Some(9_223_372_036_854_775_808),
            Some(u64::MAX),
            None,
            None,
            None,
            None
        ]
    );
    assert!(items[7].is_null());
    assert_eq!(items[6].as_str(), Some("1"));
    assert_eq!(items[8].as_bool(), Some(true));
    assert!(items[0].as_array().is_none() && items[0].as_object().is_none());
}

#[test]
fn values_keep_object_order_and_syntax_errors_stay_syntax_errors() {
    let value = parse_strict_json_value(br#"{"b":1,"a":[true,null,"x",1.5,-2]}"#).unwrap();
    assert_eq!(value.to_string(), r#"{"b":1,"a":[true,null,"x",1.5,-2]}"#);
    assert_eq!(parse_strict_json_value(b"{"), Err(StrictJsonError::Syntax));
    assert_eq!(
        parse_strict_json_value(b"{} trailing"),
        Err(StrictJsonError::Syntax)
    );
    let Ok(json) = parse_strict_json(br#"{"o":{"x":[1]}}"#, DuplicateKeys::BeforeValue) else {
        panic!("an object");
    };
    assert_eq!(
        json.get("o")
            .and_then(|o| o.get("x"))
            .and_then(Json::as_array)
            .map(<[_]>::len),
        Some(1)
    );
}
