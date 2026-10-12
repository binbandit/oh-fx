use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shade {
    Light,
    Dark,
}

impl Tag for Shade {
    const ALL: &'static [Self] = &[Self::Light, Self::Dark];

    fn name(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

#[test]
fn the_fast_path_splits_objects_as_the_writer_writes_them() {
    let fields =
        Fields::parse_object(br#"{"a":1,"b":"x,}","c":{"d":[1,"]"]},"e":null,"f":true}"#).unwrap();
    assert_eq!(fields.raw("a"), Some(&b"1"[..]));
    assert_eq!(fields.raw("b"), Some(&br#""x,}""#[..]));
    assert_eq!(fields.raw("c"), Some(&br#"{"d":[1,"]"]}"#[..]));
    assert_eq!(fields.raw("e"), Some(&b"null"[..]));
    assert_eq!(fields.opt_u64("e"), Ok(None));
    assert_eq!(fields.raw("missing"), None);
    assert!(Fields::parse_object(b"{}").unwrap().raw("a").is_none());
}

#[test]
fn unusual_objects_take_the_strict_path_with_whitespace_and_first_wins_duplicates() {
    let fields =
        Fields::parse_object(b"{ \"a\" : 1 ,\n\"a\":2, \"n\\u0061me\" : \"v\" } trailing").unwrap();
    assert_eq!(fields.req_u64("a"), Ok(1));
    assert_eq!(fields.req_str("name").as_deref(), Ok("v"));
    assert!(Fields::parse_object(b"{\"a\":1,}").is_none());
    assert!(Fields::parse_object(b"[1]").is_none());
}

#[test]
fn fields_decode_exactly_what_the_full_parse_decodes() {
    let fields = Fields::parse_object(
        br#"{"n":42,"big":"1e3","neg":-1,"s":"plain","e":"a\nb","bytes":[104,105],"tag":"dark","index":1,"text_index":"0","list":[1,"2",3e0],"bad":[256]}"#,
    )
    .unwrap();
    assert_eq!(fields.req_u64("n"), Ok(42));
    assert_eq!(fields.req_u64("big"), Ok(1000));
    assert_eq!(fields.req_u64("neg"), Err(BadBody));
    assert_eq!(fields.req_str("s").as_deref(), Ok("plain"));
    assert_eq!(fields.req_str("e").as_deref(), Ok("a\nb"));
    assert_eq!(fields.req_str("bytes").as_deref(), Ok("hi"));
    assert_eq!(fields.req_str("bad"), Err(BadBody));
    assert_eq!(fields.req_tag::<Shade>("tag"), Ok(Shade::Dark));
    assert_eq!(fields.req_tag::<Shade>("index"), Ok(Shade::Dark));
    assert_eq!(fields.req_tag::<Shade>("text_index"), Ok(Shade::Light));
    assert_eq!(fields.req_tag::<Shade>("n"), Err(BadBody));
    assert_eq!(fields.req_u64_list("list"), Ok(vec![1, 2, 3]));
    assert_eq!(fields.req_u64("missing"), Err(BadBody));
    assert_eq!(fields.opt_str("missing"), Ok(None));
}
