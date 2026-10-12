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

#[test]
fn decimals_saturate_round_and_underflow_as_std_json_does() {
    let cases: [(&str, Option<u64>); 32] = [
        ("1e9223372036854775807", None),
        ("1.0e-9223372036854775807", Some(0)),
        ("-1.0e-9223372036854775808", Some(0)),
        ("1e-9223372036854775808", Some(0)),
        ("9e99999999999999999999999", None),
        ("1e-400", None),
        ("3e-4966", Some(0)),
        ("4e-4966", None),
        (
            "0.3237587559719012555462219479113823276249784669017340504844e-4965",
            Some(0),
        ),
        (
            "0.3237587559719012555462219479113823276249784669017340504845e-4965",
            None,
        ),
        ("18446744073709551615.5", None),
        ("18446744073709551615.0000000000000001", Some(u64::MAX)),
        ("18446744073709551615.99999999999999999", None),
        ("18446744073709551614.99999999999999999", Some(u64::MAX)),
        ("0.99999999999999999999999999999999999", Some(1)),
        ("0.9999999999999999999999999999999999", None),
        ("1.00000000000000000000000000000000001", Some(1)),
        ("3.99999999999999999999999999999999999", Some(4)),
        ("123456789012345678901234567890e-10", None),
        ("1e19", Some(10_000_000_000_000_000_000)),
        ("1e20", None),
        ("1_000.5e1", Some(10_005)),
        ("1e1_0", Some(10_000_000_000)),
        ("1__0.5e1", None),
        ("1_.5", None),
        ("+.00", Some(0)),
        ("+.", None),
        (".5e1", Some(5)),
        ("1.e3", Some(1000)),
        ("-0.5", None),
        ("-1.0", None),
        ("-.000_001e-44", None),
    ];
    for (text, expected) in cases {
        assert_eq!(slice_to_u64(text), expected, "{text}");
    }
}

#[test]
fn numbers_at_exponent_and_length_extremes_agree_across_spellings() {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = |bound: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        usize::try_from(state % u64::try_from(bound).expect("bound")).expect("index")
    };
    let exponents: [i128; 18] = [
        i128::from(i64::MIN),
        i128::from(i64::MIN) + 1,
        -4967,
        -4966,
        -4965,
        -21,
        -20,
        -1,
        0,
        1,
        19,
        20,
        21,
        4965,
        i128::from(i64::MAX) - 1,
        i128::from(i64::MAX),
        i128::from(i64::MAX) + 1,
        i128::from(u64::MAX) * 10,
    ];
    let lengths: [usize; 9] = [1, 2, 19, 20, 21, 34, 35, 59, 4096];
    for _ in 0..3000 {
        let length = lengths[next(lengths.len())];
        let digits: String = (0..length)
            .map(|_| char::from(b"0123456789"[next(10)]))
            .collect();
        let exponent = if next(2) == 0 {
            exponents[next(exponents.len())]
        } else {
            i128::try_from(next(50)).expect("small") - 25
        };
        let shift = i128::try_from(length).expect("length");
        let plain = slice_to_u64(&format!("{digits}e{exponent}"));
        assert_eq!(
            slice_to_u64(&format!("0.{digits}e{}", exponent + shift)),
            plain,
            "{digits}e{exponent}"
        );
        assert_eq!(
            slice_to_u64(&format!("+000{digits}000E{}", exponent - 3)),
            plain,
            "{digits}e{exponent}"
        );
        if let (Ok(power), true) = (u32::try_from(exponent), length <= 21) {
            let exact = digits
                .parse::<u128>()
                .ok()
                .and_then(|value| match 10_u128.checked_pow(power) {
                    _ if value == 0 => Some(0),
                    Some(scale) => value.checked_mul(scale),
                    None => None,
                })
                .and_then(|value| u64::try_from(value).ok());
            assert_eq!(plain, exact, "{digits}e{exponent}");
        }
    }
}
