use super::*;

#[test]
fn keyless_json_preview_reports_shape_without_keys_or_values() {
    let preview = keyless_json_preview(
        "{\"FX_DYNAMIC_PATH\":\"secret.txt\",\"FX_DYNAMIC_CONTENT\":\"very secret\",\"nested\":{\"FX_DYNAMIC_TOKEN\":\"abc\"}}",
    );
    assert!(preview.contains("<object_fields=3 values=["));
    for hidden in [
        "FX_DYNAMIC_PATH",
        "FX_DYNAMIC_CONTENT",
        "FX_DYNAMIC_TOKEN",
        "secret.txt",
        "very secret",
        "abc",
    ] {
        assert!(!preview.contains(hidden), "{preview}");
    }
    assert_eq!(
        preview,
        "<object_fields=3 values=[<string_bytes=10>,<string_bytes=11>,<object_fields=1 values=[<string_bytes=3>]>]>"
    );
}

#[test]
fn keyless_json_preview_names_each_kind_of_value() {
    assert_eq!(
        keyless_json_preview("{\"a\":[1,2,3],\"b\":1.5,\"c\":true,\"d\":null,\"e\":\"é\"}"),
        "<object_fields=5 values=[<array_len=3>,<number>,<bool>,<null>,<string_bytes=2>]>"
    );
    assert_eq!(keyless_json_preview("[{\"a\":1}]"), "<array_len=1>");
    assert_eq!(keyless_json_preview(" \"text\" "), "<string_bytes=4>");
    assert_eq!(keyless_json_preview("{}"), "<object_fields=0>");
}

#[test]
fn keyless_json_preview_stops_two_levels_down_and_after_six_fields() {
    assert_eq!(
        keyless_json_preview("{\"a\":{\"b\":{\"c\":1,\"d\":2}}}"),
        "<object_fields=1 values=[<object_fields=1 values=[<object_fields=2>]>]>"
    );
    assert_eq!(
        keyless_json_preview("{\"a\":1,\"b\":2,\"c\":3,\"d\":4,\"e\":5,\"f\":6,\"g\":7,\"h\":8}"),
        "<object_fields=8 values=[<number>,<number>,<number>,<number>,<number>,<number>,...]>"
    );
}

#[test]
fn keyless_json_preview_reports_empty_and_invalid_text_by_size() {
    assert_eq!(keyless_json_preview(""), "<empty>");
    assert_eq!(keyless_json_preview("{\"path\":"), "<invalid-json bytes=8>");
    assert_eq!(
        keyless_json_preview("{} trailing"),
        "<invalid-json bytes=11>"
    );
}
