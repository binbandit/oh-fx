use super::*;

fn markdown(html: &str, max_output_bytes: usize) -> String {
    String::from_utf8(convert(html.as_bytes(), max_output_bytes)).unwrap()
}

#[test]
fn converts_representative_html_to_bounded_markdown() {
    let html = r#"<!doctype html>
<html>
<head><title>Ignored</title><style>.x{}</style><script>alert(1)</script></head>
<body>
<h1>Release &amp; Notes</h1>
<p>Hello <a href="/docs">docs</a> and <code>fx ask</code>.</p>
<ul><li>One</li><li>Two&nbsp;items</li></ul>
<pre><code>const x = 1 &lt; 2;</code></pre>
<table><tr><th>Name</th><th>Value</th></tr><tr><td>A</td><td>42</td></tr></table>
<img src="x.png" alt="diagram">
<p>&#169; &#x00AE; &unknown;</p>
</body></html>"#;
    let output = markdown(html, 4096);
    for expected in [
        "# Release & Notes",
        "# Ignored",
        "[docs](/docs)",
        "`fx ask`",
        "- One",
        "Two items",
        "const x = 1 < 2;",
        "| Name | Value |",
        "diagram",
        "\u{a9} \u{ae} &unknown;",
    ] {
        assert!(output.contains(expected), "{expected:?} in {output:?}");
    }
    assert!(!output.contains("alert(1)"));
    assert!(!output.contains(".x{}"));
}

#[test]
fn representative_html_converts_byte_for_byte() {
    let html = "<html><head><title> Docs  Home </title></head><body><h2>Intro</h2><p>One\n  two</p><br><ol><li>first</li></ol><pre>  keep\n   spacing  \n</pre><p>after</p></body></html>";
    assert_eq!(
        markdown(html, 4096),
        "# Docs Home\n\n## Intro\n\nOne two\n\n- first\n\n```\n  keep\n   spacing\n```\n\nafter\n"
    );
}

#[test]
fn decodes_entities_in_html_titles() {
    assert_eq!(
        markdown(
            "<html><head><title>News &amp; Updates &mdash; Site</title></head><body><p>Body</p></body></html>",
            4096,
        ),
        "# News & Updates \u{2014} Site\n\nBody\n"
    );
}

#[test]
fn malformed_html_conversion_remains_bounded() {
    let html = "<div><span>unclosed &amp; text ".repeat(2048);
    let output = markdown(&html, 1024);
    assert!(output.len() <= 1024);
    assert!(output.contains("unclosed & text"));
}

#[test]
fn numeric_entities_follow_upstream_integer_parsing() {
    assert_eq!(
        markdown("<p>&#65;&#x42;&#+67;&#6_8;&#0;&#xD800;&#-1;&#_1;</p>", 4096),
        "ABCD&#0;&#xD800;&#-1;&#_1;\n"
    );
}

#[test]
fn links_trim_their_text_and_drop_empty_anchors() {
    assert_eq!(
        markdown(
            "<p><a href='/a'>  spaced\ttext </a> <a href=/b></a><a name=x>plain</a></p>",
            4096,
        ),
        "[spaced text](/a) plain\n"
    );
    assert_eq!(
        markdown("<a href=\"/x?a=1&amp;b=2\" title=t>go</a>", 4096),
        "[go](/x?a=1&b=2)\n"
    );
}

#[test]
fn comments_declarations_and_unterminated_tags_are_handled() {
    assert_eq!(
        markdown("a<!-- hidden -->b<!DOCTYPE html><?xml?>c<span", 4096),
        "abc<span\n"
    );
    assert_eq!(markdown("before<!-- never closed", 4096), "before\n");
    assert_eq!(markdown("", 4096), "");
    assert_eq!(markdown("<p>   </p>", 4096), "");
}

#[test]
fn vertical_tabs_count_as_whitespace() {
    assert_eq!(markdown("<p>a\u{b}\u{b}b</p>", 4096), "a b\n");
}
