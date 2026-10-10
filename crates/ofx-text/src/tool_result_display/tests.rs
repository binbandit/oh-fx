use super::*;

#[test]
fn content_for_display_unwraps_path_and_content_result() {
    assert_eq!(
        content_for_display("<path>README.md</path>\n<content>\n1\talpha\n2\tbeta\n</content>"),
        "1\talpha\n2\tbeta"
    );
    assert_eq!(
        content_for_display("<content>\r\nbody\r\n</content>"),
        "body"
    );
}

#[test]
fn content_for_display_preserves_unknown_or_malformed_output() {
    let raw = "<path>README.md</path>\n<content>missing close";
    assert_eq!(content_for_display(raw), raw);
    assert_eq!(content_for_display("plain output"), "plain output");
    let unclosed = "<path>README.md\n<content>\nbody\n</content>";
    assert_eq!(content_for_display(unclosed), unclosed);
}
