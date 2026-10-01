use std::fmt::Write;
use std::time::{Duration, Instant};

use ofx_text::visible_width;

use super::*;
use crate::presentation::ansi::{HORIZONTAL_RULE_WIDTH, MAX_LINK_URL_BYTES, TABLE_HORIZ};
use crate::presentation::block_parse::is_pipe_line;
use crate::presentation::payload::{TableColumnAlign, TableRow};
use crate::styled::{Attr, Hang, Slot, Span, spans_width};
use crate::test_support::{
    assert_ansi, assert_lines, canonical, canonical_closed, contains_ansi, hangs, has_link,
    link_urls, plain_text, rendered, text_lines,
};

const LINEAR_TIME_BUDGET: Duration = Duration::from_secs(2);

fn plain_processor() -> MarkdownProcessor {
    MarkdownProcessor::with_completions(Completions::default())
}

fn push(processor: &mut MarkdownProcessor, input: &str) -> Vec<Event> {
    let mut out = Vec::new();
    processor.push(input, &mut out);
    out
}

fn render(input: &str) -> Vec<Event> {
    push(&mut plain_processor(), input)
}

fn render_flushed(input: &str) -> Vec<Event> {
    let mut processor = plain_processor();
    let mut out = Vec::new();
    processor.push(input, &mut out);
    processor.flush(&mut out);
    out
}

fn with_rules() -> MarkdownProcessor {
    MarkdownProcessor::with_completions(Completions {
        thematic_rules: true,
        ..Completions::default()
    })
}

fn with_code_blocks() -> MarkdownProcessor {
    MarkdownProcessor::with_completions(Completions {
        code_blocks: true,
        ..Completions::default()
    })
}

fn rule_count(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, Event::ThematicRule))
        .count()
}

fn code_blocks(events: &[Event]) -> Vec<CodeBlockPayload> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::CodeBlock(block) => Some(block.clone()),
            _ => None,
        })
        .collect()
}

fn rendered_line(events: &[Event], index: usize) -> String {
    rendered(events)
        .split('\n')
        .nth(index)
        .unwrap_or_default()
        .to_owned()
}

fn rendered_table(table: &TablePayload) -> String {
    let events: Vec<Event> = render_table_payload(table)
        .into_iter()
        .map(Event::Line)
        .collect();
    rendered(&events)
}

#[test]
fn assistant_presentation_event_clone_owns_nested_payloads() {
    let text = Event::Line(text_lines(&render("ok\n"))[0].clone());
    let cloned = text.clone();
    assert_eq!(cloned, text);

    let block = Event::CodeBlock(CodeBlockPayload {
        language: "zig".to_owned(),
        code: "zig".to_owned(),
    });
    assert_eq!(block.clone(), block);

    let table = parse_table_payload("| Name |\n|---|\n| api |\n");
    let table_event = Event::Table(table.clone());
    let Event::Table(cloned_table) = table_event.clone() else {
        panic!("table event clones as a table");
    };
    assert_eq!(cloned_table.rows[1].cells[0][0].text, "api");
    assert!(!text.requires_text_drain());
    assert!(table_event.requires_text_drain());
    assert!(Event::ThematicRule.requires_text_drain());
}

#[test]
fn markdown_link_is_blue_and_underlined_inside_its_osc_8_scope() {
    assert_ansi(
        &render("see [docs](https://example.com) please\n"),
        "see \x1b]8;id=fx-0;https://example.com\x1b\\\x1b[38;5;75m\x1b[4mdocs\x1b[24m\x1b[39m\x1b]8;;\x1b\\ please\n",
    );
}

#[test]
fn markdown_link_destination_keeps_balanced_parentheses() {
    assert_ansi(
        &render("[w](https://en.wikipedia.org/wiki/Foo_(bar)) tail\n"),
        "\x1b]8;id=fx-0;https://en.wikipedia.org/wiki/Foo_(bar)\x1b\\\x1b[38;5;75m\x1b[4mw\x1b[24m\x1b[39m\x1b]8;;\x1b\\ tail\n",
    );
}

#[test]
fn markdown_link_drops_its_title_and_unwraps_angle_destinations() {
    assert_ansi(
        &render(
            "[t](https://example.com \"Title text\") [s](https://example.com/s 'single') [a](<https://example.com/a b>)\n",
        ),
        concat!(
            "\x1b]8;id=fx-0;https://example.com\x1b\\\x1b[38;5;75m\x1b[4mt\x1b[24m\x1b[39m\x1b]8;;\x1b\\ ",
            "\x1b]8;id=fx-1;https://example.com/s\x1b\\\x1b[38;5;75m\x1b[4ms\x1b[24m\x1b[39m\x1b]8;;\x1b\\ ",
            "\x1b]8;id=fx-2;https://example.com/a b\x1b\\\x1b[38;5;75m\x1b[4ma\x1b[24m\x1b[39m\x1b]8;;\x1b\\\n",
        ),
    );
}

#[test]
fn markdown_link_with_unbalanced_or_spaced_destination_stays_literal() {
    let input = "[u](https://e.com/(x) [v](https://e.com/a b) [w](https://e.com \"open)\n";
    assert_ansi(&render(input), input);
}

#[test]
fn markdown_image_renders_its_alt_text_with_an_image_marker_inside_one_osc_8_scope() {
    assert_ansi(
        &render("see ![architecture diagram](https://example.com/diagram.png) please\n"),
        "see \x1b]8;id=fx-0;https://example.com/diagram.png\x1b\\\x1b[38;5;75m\x1b[4m▧ architecture diagram\x1b[24m\x1b[39m\x1b]8;;\x1b\\ please\n",
    );
}

#[test]
fn markdown_image_uses_a_stable_fallback_for_empty_alt_text() {
    assert_ansi(
        &render("![](https://example.com/diagram.png)\n"),
        "\x1b]8;id=fx-0;https://example.com/diagram.png\x1b\\\x1b[38;5;75m\x1b[4m▧ image\x1b[24m\x1b[39m\x1b]8;;\x1b\\\n",
    );
}

#[test]
fn markdown_image_unescapes_alt_punctuation_through_the_existing_link_emitter() {
    assert_ansi(
        &render("![architecture \\*diagram\\*](https://example.com/diagram.png)\n"),
        "\x1b]8;id=fx-0;https://example.com/diagram.png\x1b\\\x1b[38;5;75m\x1b[4m▧ architecture *diagram*\x1b[24m\x1b[39m\x1b]8;;\x1b\\\n",
    );
}

#[test]
fn escaped_and_malformed_markdown_images_remain_literal_without_osc_8() {
    let mut processor = plain_processor();
    let out = push(
        &mut processor,
        "\\![alt](https://example.com/diagram.png) and \\!\n",
    );
    assert_ansi(&out, "![alt](https://example.com/diagram.png) and !\n");
    assert!(!has_link(&out));

    let out = push(&mut processor, "![alt](https://example.com/diagram.png\n");
    assert_ansi(&out, "![alt](https://example.com/diagram.png\n");
    assert!(!has_link(&out));

    let out = push(
        &mut processor,
        "![alt](https://example.com/\x1bdiagram.png)\n",
    );
    assert_eq!(
        plain_text(&out),
        "![alt](https://example.com/\\x1bdiagram.png)\n"
    );
    assert!(!has_link(&out));

    let oversized = format!("![alt]({})\n", "a".repeat(MAX_LINK_URL_BYTES + 1));
    let out = push(&mut processor, &oversized);
    assert_ansi(&out, &oversized);
    assert!(!has_link(&out));
}

#[test]
fn markdown_images_preserve_code_isolation_chunk_buffering_and_heading_underline() {
    let mut processor = plain_processor();
    let out = push(
        &mut processor,
        "`![literal](https://example.com/literal.png)`\n",
    );
    assert_ansi(
        &out,
        "\x1b[38;5;245m![literal](https://example.com/literal.png)\x1b[39m\n",
    );
    assert!(!has_link(&out));

    assert!(push(&mut processor, "![architecture").is_empty());
    let out = push(&mut processor, "](https://example.com/diagram.png)\n");
    assert!(plain_text(&out).contains("▧ architecture"));

    let out = push(
        &mut processor,
        "### before ![diagram](https://example.com/diagram.png) after\n",
    );
    assert_ansi(
        &out,
        "\x1b[4mbefore \x1b]8;id=fx-0;https://example.com/diagram.png\x1b\\\x1b[38;5;75m\x1b[4m▧ diagram\x1b[24m\x1b[39m\x1b]8;;\x1b\\\x1b[4m after\x1b[24m\n",
    );
}

#[test]
fn table_payload_measures_markdown_images_by_their_visible_label() {
    let table = parse_table_payload(concat!(
        "| Asset | Status |\n",
        "| --- | --- |\n",
        "| ![diagram](https://example.com/diagram.png) | ready |\n",
    ));
    let cell = &table.rows[1].cells[0];
    let text: String = cell.iter().map(|span| span.text.as_str()).collect();
    assert!(text.contains("▧ diagram"));
    assert!(!text.contains("![diagram]"));
    assert_eq!(visible_width("▧ diagram"), spans_width(cell));
}

#[test]
fn table_payload_links_use_the_shared_osc_8_identifier_sequence() {
    let first = parse_table_payload("| Link |\n|------|\n| [first](https://first.example) |\n");
    let second = parse_table_payload("| Link |\n|------|\n| [second](https://second.example) |\n");
    let first_link = first.rows[1].cells[0][0]
        .link
        .clone()
        .expect("first cell links");
    let second_link = second.rows[1].cells[0][0]
        .link
        .clone()
        .expect("second cell links");
    assert_eq!(first_link.url, "https://first.example");
    assert_eq!(second_link.url, "https://second.example");
    assert!(second_link.id > first_link.id);
}

#[test]
fn bare_url_is_underlined_and_leaves_sentence_punctuation_literal() {
    assert_ansi(
        &render("visit https://example.com/docs, now\n"),
        "visit \x1b]8;id=fx-0;https://example.com/docs\x1b\\\x1b[38;5;75m\x1b[4mhttps://example.com/docs\x1b[24m\x1b[39m\x1b]8;;\x1b\\, now\n",
    );
}

#[test]
fn bare_url_is_recognized_after_streamed_input_chunks() {
    let mut processor = plain_processor();
    assert!(push(&mut processor, "visit https").is_empty());
    assert_ansi(
        &push(&mut processor, "://example.com/docs\n"),
        "visit \x1b]8;id=fx-0;https://example.com/docs\x1b\\\x1b[38;5;75m\x1b[4mhttps://example.com/docs\x1b[24m\x1b[39m\x1b]8;;\x1b\\\n",
    );
}

#[test]
fn angle_autolinks_use_literal_uri_and_email_labels() {
    assert_ansi(
        &render(
            "see <https://example.com/docs\\_literal> and <dev@example.com> plus <git+ssh://example.com/repo>\n",
        ),
        concat!(
            "see \x1b]8;id=fx-0;https://example.com/docs\\_literal\x1b\\\x1b[38;5;75m\x1b[4mhttps://example.com/docs\\_literal\x1b[24m\x1b[39m\x1b]8;;\x1b\\ and ",
            "\x1b]8;id=fx-1;mailto:dev@example.com\x1b\\\x1b[38;5;75m\x1b[4mdev@example.com\x1b[24m\x1b[39m\x1b]8;;\x1b\\ plus ",
            "\x1b]8;id=fx-2;git+ssh://example.com/repo\x1b\\\x1b[38;5;75m\x1b[4mgit+ssh://example.com/repo\x1b[24m\x1b[39m\x1b]8;;\x1b\\\n",
        ),
    );
}

#[test]
fn rejected_angle_candidates_suppress_nested_link_emission() {
    let input = concat!(
        "<not-an-autolink https://inner.example>\n",
        "\\<not-an-autolink https://inner.example>\n",
        "<<https://inner.example>>\n",
        "<[inner](https://inner.example)>\n",
        "<[inner](https://one.example) https://two.example>\n",
        "\\<https://escaped.example>\n",
        "<a:x> <dev@bad-.example> <https://has space>\n",
        "<not-an-autolink https://unterminated.example\n",
        "`<https://code.example>`\n",
    );
    let expected = concat!(
        "<not-an-autolink https://inner.example>\n",
        "<not-an-autolink https://inner.example>\n",
        "<<https://inner.example>>\n",
        "<[inner](https://inner.example)>\n",
        "<[inner](https://one.example) https://two.example>\n",
        "<https://escaped.example>\n",
        "<a:x> <dev@bad-.example> <https://has space>\n",
        "<not-an-autolink https://unterminated.example\n",
        "\x1b[38;5;245m<https://code.example>\x1b[39m\n",
    );
    let out = render(input);
    assert_ansi(&out, expected);
    assert!(!has_link(&out));
}

#[test]
fn angle_email_autolink_applies_the_destination_cap_including_mailto() {
    let suffix = "@a.com";
    let local_len = MAX_LINK_URL_BYTES - "mailto:".len() - suffix.len() + 1;
    let input = format!("<{}{suffix}>\n", "a".repeat(local_len));
    let out = render(&input);
    assert_ansi(&out, &input);
    assert!(!has_link(&out));
}

#[test]
fn standalone_url_in_code_is_linked_while_excluded_boundaries_stay_literal() {
    assert_ansi(
        &render("`https://code.example` wordhttps://word.example <<https://angle.example>>\n"),
        "\x1b[38;5;245m\x1b]8;id=fx-0;https://code.example\x1b\\https://code.example\x1b]8;;\x1b\\\x1b[39m wordhttps://word.example <<https://angle.example>>\n",
    );
}

#[test]
fn inline_code_urls_keep_literal_bytes_and_trailing_punctuation_outside_links() {
    assert_ansi(
        &render("See `https://example.com/path_~*.` and `https://example.com/a\\_b`.\n"),
        concat!(
            "See \x1b[38;5;245m\x1b]8;id=fx-0;https://example.com/path_~*\x1b\\https://example.com/path_~*\x1b]8;;\x1b\\.\x1b[39m and ",
            "\x1b[38;5;245m\x1b]8;id=fx-1;https://example.com/a\\_b\x1b\\https://example.com/a\\_b\x1b]8;;\x1b\\\x1b[39m.\n",
        ),
    );
}

#[test]
fn inline_code_urls_preserve_literal_trailing_uri_punctuation() {
    for url in [
        "https://example.com/a!",
        "https://example.com/a?",
        "https://example.com/a;",
        "https://example.com/a:",
        "https://example.com/a,",
    ] {
        assert_ansi(
            &render(&format!("Open `{url}`\n")),
            &format!("Open \x1b[38;5;245m\x1b]8;id=fx-0;{url}\x1b\\{url}\x1b]8;;\x1b\\\x1b[39m\n"),
        );
    }
}

#[test]
fn non_url_and_unsafe_inline_code_stays_literal() {
    for input in [
        "`not a URL`\n",
        "`curl https://example.com`\n",
        "`https://example.com more`\n",
        "`http://`\n",
        "`https://example.com\x07`\n",
        "`<https://example.com>`\n",
    ] {
        assert!(!has_link(&render(input)), "{input:?}");
    }
    let oversized = format!("`https://{}`\n", "a".repeat(MAX_LINK_URL_BYTES + 1));
    assert!(!has_link(&render(&oversized)));
}

#[test]
fn unsafe_bare_url_renders_literally() {
    let input = format!("https://{}\n", "a".repeat(MAX_LINK_URL_BYTES + 1));
    let out = render(&input);
    assert_ansi(&out, &input);
    assert!(!has_link(&out));
}

#[test]
fn bare_urls_leave_closing_emphasis_delimiters_for_the_inline_scanner() {
    assert_ansi(
        &render(
            "**https://bold.example** *https://italic.example* ~~https://strike.example~~ tail\n",
        ),
        concat!(
            "\x1b[1m\x1b]8;id=fx-0;https://bold.example\x1b\\\x1b[38;5;75m\x1b[4mhttps://bold.example\x1b[24m\x1b[39m\x1b]8;;\x1b\\\x1b[22m ",
            "\x1b[3m\x1b]8;id=fx-1;https://italic.example\x1b\\\x1b[38;5;75m\x1b[4mhttps://italic.example\x1b[24m\x1b[39m\x1b]8;;\x1b\\\x1b[23m ",
            "\x1b[9m\x1b]8;id=fx-2;https://strike.example\x1b\\\x1b[38;5;75m\x1b[4mhttps://strike.example\x1b[24m\x1b[39m\x1b]8;;\x1b\\\x1b[29m tail\n",
        ),
    );
}

#[test]
fn malformed_formatted_link_suppresses_bare_url_recognition() {
    let input = "see [docs](https://example.com missing close\n";
    let out = render(input);
    assert_ansi(&out, input);
    assert!(!has_link(&out));
}

#[test]
fn heading_underline_resumes_after_a_link_closes_its_local_underline() {
    assert_ansi(
        &render("### before [link](https://example.com) after\nbody\n"),
        "\x1b[4mbefore \x1b]8;id=fx-0;https://example.com\x1b\\\x1b[38;5;75m\x1b[4mlink\x1b[24m\x1b[39m\x1b]8;;\x1b\\\x1b[4m after\x1b[24m\nbody\n",
    );
}

#[test]
fn two_markdown_links_get_distinct_ids() {
    let links = link_urls(&render(
        "[a](https://a.example) and [b](https://b.example)\n",
    ));
    assert_eq!(links.len(), 2);
    assert_ne!(links[0].0, links[1].0);
}

#[test]
fn url_over_osc_8_size_cap_renders_literally() {
    let url = "a".repeat(MAX_LINK_URL_BYTES + 1);
    let out = render(&format!("see [x]({url}) end\n"));
    assert!(!has_link(&out));
    assert!(plain_text(&out).contains("[x]("));
}

#[test]
fn unmatched_bracket_renders_literally() {
    assert_ansi(
        &render("list [1, 2, 3] of items\n"),
        "list [1, 2, 3] of items\n",
    );
}

#[test]
fn link_with_empty_url_renders_literally() {
    assert_ansi(&render("see [docs]() now\n"), "see [docs]() now\n");
}

#[test]
fn plain_text_passes_through_unchanged() {
    assert_ansi(&render("hello world\n"), "hello world\n");
}

#[test]
fn backslash_escapes_keep_inline_punctuation_literal() {
    let out = render(
        "literal \\*em\\* \\*\\*bold\\*\\* \\_italic\\_ \\_\\_strong\\_\\_ \\~\\~strike\\~\\~ \\`code\\` \\[docs](https://example.com) \\\\ \\| \\! \\# \\>\n",
    );
    assert_ansi(
        &out,
        "literal *em* **bold** _italic_ __strong__ ~~strike~~ `code` [docs](https://example.com) \\ | ! # >\n",
    );
    assert!(!rendered(&out).contains('\x1b'));
}

#[test]
fn backslash_escapes_survive_heading_preprocessing_and_code_spans() {
    assert_ansi(
        &render("## \\*\\*literal bold\\*\\* and `\\*code\\*`\n"),
        "\x1b[1m**literal bold** and \x1b[38;5;245m\\*code\\*\x1b[39m\x1b[22m\n",
    );
}

#[test]
fn completed_markdown_lines_consume_one_terminal_hard_break_backslash() {
    assert_ansi(
        &render(concat!(
            "plain\\\n",
            "# heading\\\n",
            "- list\\\n",
            "> quote\\\n",
            "two\\\\\n",
            "three\\\\\\\n",
        )),
        concat!(
            "plain\n",
            "\x1b[1m\x1b[4mheading\x1b[24m\x1b[22m\n",
            "\x1b[2m• \x1b[22mlist\n",
            "\x1b[2m│ \x1b[22mquote\n",
            "\x1b[2m│ \x1b[22mtwo\\\n",
            "\x1b[2m│ \x1b[22mthree\\\n",
        ),
    );
}

#[test]
fn terminal_hard_break_backslashes_preserve_eof_and_code_content() {
    let mut processor = with_rules();
    let mut out = Vec::new();
    processor.push("eof\\", &mut out);
    processor.flush(&mut out);
    assert_ansi(&out, "eof\\");

    assert_ansi(&render("```\ncode\\\n```\n"), "\x1b[2m│ \x1b[22mcode\\\n");
}

#[test]
fn setext_and_invalid_pipe_fallback_retain_source_line_completion() {
    let out = push(&mut with_rules(), "Setext\\\n---\n");
    assert_ansi(&out, "\x1b[1mSetext\x1b[22m\n");
    assert_eq!(rule_count(&out), 0);

    assert_ansi(&render_flushed("| prior\\\n| eof\\"), "| prior\n| eof\\");
}

#[test]
fn link_labels_unescape_visible_punctuation() {
    assert_ansi(
        &render("[docs \\*literal\\*](https://example.com)\n"),
        "\x1b]8;id=fx-0;https://example.com\x1b\\\x1b[38;5;75m\x1b[4mdocs *literal*\x1b[24m\x1b[39m\x1b]8;;\x1b\\\n",
    );
}

#[test]
fn backslash_escapes_stay_literal_in_setext_headings() {
    let out = push(&mut with_rules(), "\\*\\*literal bold\\*\\*\n---\n");
    assert_ansi(&out, "\x1b[1m**literal bold**\x1b[22m\n");
    assert_eq!(rule_count(&out), 0);
}

#[test]
fn bold_double_asterisk_wraps_with_ansi() {
    assert_ansi(
        &render("hi **there** friend\n"),
        "hi \x1b[1mthere\x1b[22m friend\n",
    );
}

#[test]
fn italic_single_asterisk_wraps_with_ansi() {
    assert_ansi(
        &render("use *this* quickly\n"),
        "use \x1b[3mthis\x1b[23m quickly\n",
    );
}

#[test]
fn underscore_emphasis_styles_valid_spans_and_preserves_invalid_markers() {
    assert_ansi(
        &render(
            "paragraph _italic_ and __bold__ with snake_case, snake__case, _ spaced_, and __ spaced__.\n",
        ),
        "paragraph \x1b[3mitalic\x1b[23m and \x1b[1mbold\x1b[22m with snake_case, snake__case, _ spaced_, and __ spaced__.\n",
    );
}

#[test]
fn underscore_emphasis_renders_in_list_blockquote_and_table_cells() {
    assert_ansi(
        &render("- _item_\n> __quote__\n"),
        "\x1b[2m• \x1b[22m\x1b[3mitem\x1b[23m\n\x1b[2m│ \x1b[22m\x1b[1mquote\x1b[22m\n",
    );

    let table = parse_table_payload("| Name | Value |\n|------|-------|\n| _row_ | __cell__ |\n");
    let rendered = rendered_table(&table);
    assert!(rendered.contains(&canonical("\x1b[3mrow\x1b[23m")));
    assert!(rendered.contains(&canonical("\x1b[1mcell\x1b[22m")));
}

#[test]
fn underscore_formatted_bare_urls_retain_path_underscores_and_matching_closers() {
    assert_ansi(
        &render("_https://example.com/snake_case_ tail __https://example.com/snake_case__ tail\n"),
        concat!(
            "\x1b[3m\x1b]8;id=fx-0;https://example.com/snake_case\x1b\\\x1b[38;5;75m\x1b[4mhttps://example.com/snake_case\x1b[24m\x1b[39m\x1b]8;;\x1b\\\x1b[23m tail ",
            "\x1b[1m\x1b]8;id=fx-1;https://example.com/snake_case\x1b\\\x1b[38;5;75m\x1b[4mhttps://example.com/snake_case\x1b[24m\x1b[39m\x1b]8;;\x1b\\\x1b[22m tail\n",
        ),
    );
}

#[test]
fn underscore_formatted_urls_require_exact_active_markers() {
    for input in [
        "snake_https://example.com\n",
        "snake__https://example.com\n",
        "_prefix__https://example.com\n",
        "__prefix__https://example.com\n",
    ] {
        assert!(!has_link(&render(input)), "{input:?}");
    }

    let link = "\x1b]8;id=fx-0;https://example.com/path\x1b\\\x1b[38;5;75m\x1b[4mhttps://example.com/path\x1b[24m\x1b[39m\x1b]8;;\x1b\\";
    assert_ansi(
        &render("_https://example.com/path__ tail\n"),
        &format!("\x1b[3m{link}\x1b[23m_ tail\n"),
    );
    assert_ansi(
        &render("__https://example.com/path_ tail\n"),
        &format!("_\x1b[3m{link}\x1b[23m tail\n"),
    );
}

#[test]
fn underscore_emphasis_preserves_code_literals_and_keeps_unpaired_markers_literal() {
    assert_ansi(
        &render("_unclosed\n__unclosed\n`_literal_ __literal__`\n"),
        "_unclosed\n__unclosed\n\x1b[38;5;245m_literal_ __literal__\x1b[39m\n",
    );
}

#[test]
fn headings_preserve_literal_underscores_and_suppress_valid_double_underscore_strong_markers() {
    assert_ansi(
        &render("## __Strong__ snake__case __ spaced__\n"),
        "\x1b[1mStrong snake__case __ spaced__\x1b[22m\n",
    );
}

#[test]
fn cross_form_nested_strong_spans_reassert_the_remaining_bold_style() {
    assert_ansi(
        &render("**outer __inner__ suffix** __outer **inner** suffix__\n"),
        concat!(
            "\x1b[1mouter \x1b[1minner\x1b[22m\x1b[1m suffix\x1b[22m ",
            "\x1b[1mouter \x1b[1minner\x1b[22m\x1b[1m suffix\x1b[22m\n",
        ),
    );
}

#[test]
fn cross_form_nested_italic_spans_reassert_the_remaining_italic_style() {
    assert_ansi(
        &render("*outer _inner_ suffix* _outer *inner* suffix_\n"),
        concat!(
            "\x1b[3mouter \x1b[3minner\x1b[23m\x1b[3m suffix\x1b[23m ",
            "\x1b[3mouter \x1b[3minner\x1b[23m\x1b[3m suffix\x1b[23m\n",
        ),
    );
}

#[test]
fn table_payload_headers_reassert_outer_bold_after_inline_strong_closes() {
    let table = parse_table_payload("| prefix __strong__ suffix |\n|------|\n| value |\n");
    assert!(rendered_table(&table).contains(&canonical(
        "\x1b[1mprefix \x1b[1mstrong\x1b[22m\x1b[1m suffix\x1b[22m"
    )));
}

#[test]
fn double_backtick_code_span_keeps_inner_backticks_and_trims_one_padding_space() {
    assert_ansi(
        &render("use `` a ` b `` and ``x`` here\n"),
        "use \x1b[38;5;245ma ` b\x1b[39m and \x1b[38;5;245mx\x1b[39m here\n",
    );
}

#[test]
fn code_span_closes_only_on_a_backtick_run_of_the_same_length() {
    assert_ansi(
        &render("`a``b` and ``` lonely **bold**\n"),
        "\x1b[38;5;245ma``b\x1b[39m and ``` lonely \x1b[1mbold\x1b[22m\n",
    );
}

#[test]
fn numeric_entities_for_control_characters_stay_literal() {
    let out = render("x&#27;[2Ky &#x1b;[31m &#7; &#127;&#x9b; &#0; &#x41;\n");
    assert_ansi(&out, "x&#27;[2Ky &#x1b;[31m &#7; &#127;&#x9b; \u{fffd} A\n");
    assert!(!plain_text(&out).contains('\x1b'));
}

#[test]
fn entity_lookup_is_bounded_and_a_long_ampersand_line_renders_in_bounded_time() {
    let mut processor = plain_processor();
    assert_ansi(
        &push(&mut processor, "&ampersand; &amp;\n"),
        "&ampersand; &\n",
    );

    let line = format!("{}\n", "&".repeat(64 * 1024));
    let started = Instant::now();
    let out = push(&mut processor, &line);
    assert!(started.elapsed() < LINEAR_TIME_BUDGET);
    assert_eq!(plain_text(&out), line);
}

#[test]
fn code_span_content_is_not_entity_decoded_but_prose_is() {
    assert_ansi(
        &render("a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#39; &#x2192; `&amp;` &unknown; &amp\n"),
        "a & b <c> \"d\" 'e' → \x1b[38;5;245m&amp;\x1b[39m &unknown; &amp\n",
    );
}

#[test]
fn heading_strips_emphasis_markers_around_a_multi_backtick_code_span() {
    assert_ansi(
        &render("## Run ``**raw**`` now\n"),
        "\x1b[1mRun \x1b[38;5;245m**raw**\x1b[39m now\x1b[22m\n",
    );
}

#[test]
fn inline_code_backticks_wrap_with_ansi() {
    assert_ansi(
        &render("run `zig build` now\n"),
        "run \x1b[38;5;245mzig build\x1b[39m now\n",
    );
}

#[test]
fn stray_asterisk_between_spaces_stays_literal() {
    assert_ansi(&render("3 * 5 = 15\n"), "3 * 5 = 15\n");
}

#[test]
fn headings_use_level_specific_ansi_styles() {
    let cases = [
        (
            "# Workspace overview\n",
            "\x1b[1m\x1b[4mWorkspace overview\x1b[24m\x1b[22m\n",
        ),
        ("## Installation\n", "\x1b[1mInstallation\x1b[22m\n"),
        ("### macOS\n", "\x1b[4mmacOS\x1b[24m\n"),
        ("#### Shell setup\n", "\x1b[1m\x1b[2mShell setup\x1b[22m\n"),
        (
            "##### Optional tools\n",
            "\x1b[2m\x1b[4mOptional tools\x1b[24m\x1b[22m\n",
        ),
        (
            "###### Troubleshooting\n",
            "\x1b[2mTroubleshooting\x1b[22m\n",
        ),
    ];
    for (input, expected) in cases {
        assert_ansi(&render(input), expected);
    }
}

#[test]
fn setext_headings_use_atx_styles_and_bypass_thematic_rule_completion() {
    let mut processor = with_rules();
    let mut out = Vec::new();
    processor.push("Workspace *overview*\n", &mut out);
    assert!(out.is_empty());

    processor.push("===\nInstallation **guide**\n---\n", &mut out);
    assert_ansi(
        &out,
        concat!(
            "\x1b[1m\x1b[4mWorkspace \x1b[3moverview\x1b[23m\x1b[24m\x1b[22m\n",
            "\x1b[1mInstallation guide\x1b[22m\n",
        ),
    );
    assert_eq!(rule_count(&out), 0);

    let out = push(&mut processor, "\n---\n");
    assert_eq!(rule_count(&out), 1);
}

#[test]
fn setext_candidate_flushes_at_eof() {
    let mut processor = with_rules();
    let mut out = Vec::new();
    processor.push("ordinary title\n", &mut out);
    assert!(out.is_empty());
    processor.flush(&mut out);
    assert_ansi(&out, "ordinary title\n");
}

#[test]
fn definition_lists_render_adjacent_markers_through_thematic_completion() {
    let mut processor = with_rules();
    let mut out = Vec::new();
    processor.push("Status\n: **Running**\n: second definition\n", &mut out);
    processor.flush(&mut out);
    assert_ansi(
        &out,
        concat!(
            "Status\n",
            "\x1b[2m  \x1b[22m\x1b[1mRunning\x1b[22m\n",
            "\x1b[2m  \x1b[22msecond definition\n",
        ),
    );
}

fn definition_output(input: &str) -> Vec<Event> {
    let mut processor = with_rules();
    let mut out = Vec::new();
    processor.push(input, &mut out);
    processor.flush(&mut out);
    out
}

#[test]
fn definition_state_stops_before_a_fenced_code_block() {
    let out = definition_output("Status\n: accepted\n```\ncode\n```\n: stale\n");
    assert!(contains_ansi(&out, "\x1b[2m  \x1b[22maccepted\n"));
    assert!(contains_ansi(&out, "\n: stale\n"));
}

#[test]
fn definition_state_stops_before_a_pipe_table() {
    let out = definition_output("Status\n: accepted\n| Name |\n| --- |\n| value |\n: stale\n");
    assert!(contains_ansi(&out, "\x1b[2m  \x1b[22maccepted\n"));
    assert!(contains_ansi(&out, "\n: stale\n"));
}

#[test]
fn definition_marker_cannot_skip_a_fenced_code_block() {
    let out = definition_output("Status\n```\ncode\n```\n: stale\n");
    assert!(contains_ansi(&out, "Status\n"));
    assert!(contains_ansi(&out, "\n: stale\n"));
}

#[test]
fn definition_marker_cannot_skip_a_pipe_table() {
    let out = definition_output("Status\n| Name |\n| --- |\n| value |\n: stale\n");
    assert!(contains_ansi(&out, "Status\n"));
    assert!(contains_ansi(&out, "\n: stale\n"));
}

#[test]
fn definition_markers_without_a_term_stay_literal() {
    assert_ansi(
        &definition_output(": orphan\n: second marker\n"),
        ": orphan\n: second marker\n",
    );
}

#[test]
fn definition_markers_require_a_separator_and_adjacency() {
    assert_ansi(
        &definition_output("Status\n:no-space\nEmpty\n: \t\nSeparated\n\n: body\n"),
        "Status\n:no-space\nEmpty\n: \t\nSeparated\n\n: body\n",
    );
    assert_ansi(&render("Direct\n: body\n"), "Direct\n: body\n");
}

#[test]
fn definition_bodies_preserve_lf_and_eof_hard_break_behavior() {
    assert_ansi(
        &definition_output("Status\n: line\\\n: eof\\"),
        "Status\n\x1b[2m  \x1b[22mline\n\x1b[2m  \x1b[22meof\\",
    );
}

#[test]
fn setext_lookahead_leaves_structural_predecessors_as_standalone_rules() {
    let out = push(
        &mut with_rules(),
        "# ATX heading\n---\n- list item\n---\n> quoted text\n---\nordinary prose\n___\n",
    );
    assert_eq!(rule_count(&out), 4);
    assert_ansi(
        &out,
        concat!(
            "\x1b[1m\x1b[4mATX heading\x1b[24m\x1b[22m\n",
            "\x1b[2m• \x1b[22mlist item\n",
            "\x1b[2m│ \x1b[22mquoted text\n",
            "ordinary prose\n",
        ),
    );
}

#[test]
fn footnote_references_emit_dim_markers_and_defer_definitions() {
    assert_ansi(
        &render_flushed(concat!(
            "The cache is scoped to this request.[^cache]\n",
            "[^cache]: It is discarded after the request completes.\n",
        )),
        concat!(
            "The cache is scoped to this request.\x1b[2m[1]\x1b[22m\n\n",
            "\x1b[2m[1] \x1b[22mIt is discarded after the request completes.\n",
        ),
    );
}

#[test]
fn table_cell_footnote_references_project_deferred_definitions() {
    let out = render_flushed(concat!(
        "| Component | State |\n",
        "| --- | --- |\n",
        "| api | [^state] |\n",
        "[^state]: The state comes from the deployment record.\n",
    ));
    assert!(!plain_text(&out).contains("[^state]"));
    assert!(contains_ansi(&out, "\x1b[2m[1] \x1b[22m"));
    assert!(rendered(&out).ends_with(&canonical(
        "\n\n\x1b[2m[1] \x1b[22mThe state comes from the deployment record.\n"
    )));
}

#[test]
fn footnotes_normalize_the_final_separator_after_blank_source_lines() {
    assert_ansi(
        &render_flushed(concat!(
            "The cache is scoped to this request.[^cache]\n\n",
            "[^cache]: It is discarded after the request completes.\n",
        )),
        concat!(
            "The cache is scoped to this request.\x1b[2m[1]\x1b[22m\n\n",
            "\x1b[2m[1] \x1b[22mIt is discarded after the request completes.\n",
        ),
    );
}

#[test]
fn footnotes_use_first_reference_order_and_keep_the_first_definition() {
    assert_ansi(
        &render_flushed(concat!(
            "[^second]: The second definition arrives first.\n",
            "[^first]: The first definition arrives second.\n",
            "First[^first], second[^second], and first again[^first].\n",
            "[^first]: This duplicate definition is ignored.\n",
        )),
        concat!(
            "First\x1b[2m[1]\x1b[22m, second\x1b[2m[2]\x1b[22m, and first again\x1b[2m[1]\x1b[22m.\n\n",
            "\x1b[2m[1] \x1b[22mThe first definition arrives second.\n",
            "\x1b[2m[2] \x1b[22mThe second definition arrives first.\n",
        ),
    );
}

#[test]
fn footnote_definitions_format_multiline_bodies_and_retain_eof_separators() {
    assert_ansi(
        &render_flushed(concat!(
            "[^cache]: First **formatted** line.\n",
            "  Second continuation line.\n",
            "\tThird continuation line.\n",
            "The cache is scoped to this request.[^cache]",
        )),
        concat!(
            "The cache is scoped to this request.\x1b[2m[1]\x1b[22m\n\n",
            "\x1b[2m[1] \x1b[22mFirst \x1b[1mformatted\x1b[22m line.\n",
            "\x1b[2m    \x1b[22mSecond continuation line.\n",
            "\x1b[2m    \x1b[22mThird continuation line.\n",
        ),
    );
}

#[test]
fn escaped_malformed_and_code_footnote_candidates_remain_literal() {
    let out =
        render_flushed("\\[^escaped] `[^code]` [^] [^missing]:\n\n    [^block]: literal code\n");
    let text = plain_text(&out);
    assert!(!text.contains("[1]"));
    assert!(text.contains("[^escaped]"));
    assert!(text.contains("[^code]"));
    assert!(text.contains("[^missing]:"));
    assert!(text.contains("[^block]: literal code"));
}

#[test]
fn heading_keeps_inline_emphasis_without_nested_bold_markers() {
    assert_ansi(
        &render("# **Strong** *emphasis*\n"),
        "\x1b[1m\x1b[4mStrong \x1b[3memphasis\x1b[23m\x1b[24m\x1b[22m\n",
    );
}

#[test]
fn hash_without_trailing_space_is_literal() {
    assert_ansi(&render("#notATag\n"), "#notATag\n");
}

#[test]
fn unordered_list_dash_gets_bullet() {
    assert_ansi(&render("- first item\n"), "\x1b[2m• \x1b[22mfirst item\n");
}

#[test]
fn unordered_list_asterisk_gets_bullet() {
    assert_ansi(&render("* second item\n"), "\x1b[2m• \x1b[22msecond item\n");
}

#[test]
fn unordered_list_literal_bullet_gets_dim_marker() {
    assert_ansi(
        &render("• third item\n  • nested\n"),
        "\x1b[2m• \x1b[22mthird item\n  \x1b[2m• \x1b[22mnested\n",
    );
}

#[test]
fn unordered_list_plus_marker_and_tab_separator_get_bullet() {
    assert_ansi(
        &render("+ plus item\n-\ttabbed item\n+not a list\n"),
        "\x1b[2m• \x1b[22mplus item\n\x1b[2m• \x1b[22mtabbed item\n+not a list\n",
    );
}

#[test]
fn ordered_list_accepts_paren_markers_and_rejects_long_numbers() {
    assert_ansi(
        &render("1) first\n12) twelfth\n1234567890. too long\n"),
        "\x1b[2m1)\x1b[22m first\n\x1b[2m12)\x1b[22m twelfth\n1234567890. too long\n",
    );
}

#[test]
fn atx_heading_strips_closing_hashes_and_allows_small_indent() {
    assert_ansi(
        &render("## Title ##\n   ## Indented\n## Keep#\n## Trail ##   \n    ## code\n"),
        concat!(
            "\x1b[1mTitle\x1b[22m\n",
            "\x1b[1mIndented\x1b[22m\n",
            "\x1b[1mKeep#\x1b[22m\n",
            "\x1b[1mTrail\x1b[22m\n",
            "    ## code\n",
        ),
    );
}

#[test]
fn longer_code_fence_contains_shorter_fences() {
    let mut processor = plain_processor();
    let out = push(&mut processor, "````md\n```zig\ninner\n```\n````\nafter\n");
    assert_ansi(
        &out,
        concat!(
            "\x1b[2m│ \x1b[22m```zig\n",
            "\x1b[2m│ \x1b[22minner\n",
            "\x1b[2m│ \x1b[22m```\n",
            "after\n",
        ),
    );
    assert!(processor.code_block.is_none());
}

#[test]
fn fenced_code_inside_a_list_item_drops_the_item_indentation() {
    assert_ansi(
        &render("1. step\n   ```sh\n   ls -la\n     nested\n   ```\n- next\n"),
        concat!(
            "\x1b[2m1.\x1b[22m step\n",
            "\x1b[2m│ \x1b[22mls -la\n",
            "\x1b[2m│ \x1b[22m  nested\n",
            "\x1b[2m• \x1b[22mnext\n",
        ),
    );
}

#[test]
fn tab_indented_fence_inside_a_list_item_closes_on_a_tab_indented_fence() {
    assert_ansi(
        &render("1. item\n\t```\n\tcode\n\t```\n\tprose after\n"),
        "\x1b[2m1.\x1b[22m item\n\x1b[2m│ \x1b[22mcode\n\tprose after\n",
    );
}

#[test]
fn indented_plus_line_after_a_blank_stays_indented_code() {
    assert_ansi(
        &render_flushed("    + value\n    second\ntext\n- a\n    + nested\n"),
        concat!(
            "\x1b[2m│ \x1b[22m+ value\n",
            "\x1b[2m│ \x1b[22msecond\n",
            "text\n",
            "\x1b[2m• \x1b[22ma\n",
            "    \x1b[2m• \x1b[22mnested\n",
        ),
    );
}

#[test]
fn heading_keeps_a_backslash_exposed_by_removing_closing_hashes() {
    assert_ansi(
        &render("## C:\\ ###\n## trailing\\\n> ## C:\\ ###\n> ## quoted\\\n"),
        concat!(
            "\x1b[1mC:\\\x1b[22m\n",
            "\x1b[1mtrailing\x1b[22m\n",
            "\x1b[2m│ \x1b[22m\x1b[1mC:\\\x1b[22m\n",
            "\x1b[2m│ \x1b[22m\x1b[1mquoted\x1b[22m\n",
        ),
    );
}

#[test]
fn blockquote_renders_headings_and_list_items_inside_the_quote() {
    assert_ansi(
        &render("> ## Note\n> - one\n> 2. two\n> - [x] done\n> plain\n"),
        concat!(
            "\x1b[2m│ \x1b[22m\x1b[1mNote\x1b[22m\n",
            "\x1b[2m│ \x1b[22m\x1b[2m• \x1b[22mone\n",
            "\x1b[2m│ \x1b[22m\x1b[2m2.\x1b[22m two\n",
            "\x1b[2m│ \x1b[22m\x1b[38;5;252m✓\x1b[39m done\n",
            "\x1b[2m│ \x1b[22mplain\n",
        ),
    );
}

#[test]
fn literal_bullet_without_trailing_space_stays_prose() {
    assert_ansi(&render("•item\n•\n"), "•item\n•\n");
}

#[test]
fn ordered_list_keeps_number_marker() {
    assert_ansi(&render("1. step one\n"), "\x1b[2m1.\x1b[22m step one\n");
}

#[test]
fn task_list_items_render_static_pending_and_completed_markers() {
    assert_ansi(
        &render("- [ ] pending **task**\n- [x] done\n  * [X] nested done\n- [ ]\n"),
        concat!(
            "\x1b[2m☐ \x1b[22mpending \x1b[1mtask\x1b[22m\n",
            "\x1b[38;5;252m✓\x1b[39m done\n",
            "  \x1b[38;5;252m✓\x1b[39m nested done\n",
            "\x1b[2m☐\x1b[22m\n",
        ),
    );
}

#[test]
fn ordered_task_list_items_retain_their_number_markers() {
    assert_ansi(
        &render("1. [ ] first\n2. [X] done\n"),
        concat!(
            "\x1b[2m1.\x1b[22m \x1b[2m☐ \x1b[22mfirst\n",
            "\x1b[2m2.\x1b[22m \x1b[38;5;252m✓\x1b[39m done\n",
        ),
    );
}

#[test]
fn task_list_syntax_remains_literal_outside_its_bounded_grammar() {
    assert_ansi(
        &render(concat!(
            "- [x]done\n",
            "- [-] unsupported\n",
            "- [ ]pending\n",
            "ordinary [x] prose\n",
            "```text\n",
            "- [x] literal code\n",
            "```\n",
        )),
        concat!(
            "\x1b[2m• \x1b[22m[x]done\n",
            "\x1b[2m• \x1b[22m[-] unsupported\n",
            "\x1b[2m• \x1b[22m[ ]pending\n",
            "ordinary [x] prose\n",
            "\x1b[2m│ \x1b[22m- [x] literal code\n",
        ),
    );
}

#[test]
fn task_list_prefix_is_retained_across_streamed_input_chunks() {
    let mut processor = plain_processor();
    assert!(push(&mut processor, "- [x]").is_empty());
    assert_ansi(
        &push(&mut processor, " streamed task\n"),
        "\x1b[38;5;252m✓\x1b[39m streamed task\n",
    );
}

#[test]
fn blockquote_gets_a_dim_vertical_rule_and_renders_inline_markdown() {
    assert_ansi(
        &render("  > **important** note\n"),
        "  \x1b[2m│ \x1b[22m\x1b[1mimportant\x1b[22m note\n",
    );
}

#[test]
fn lazy_nested_blockquote_continuations_retain_their_rules() {
    let mut processor = plain_processor();
    let mut out = Vec::new();
    processor.push("> > first **quoted** line\n", &mut out);
    processor.push(
        "lazy second line\n> > explicit third line\nlazy fourth line\n\noutside\n",
        &mut out,
    );
    assert_ansi(
        &out,
        concat!(
            "\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22mfirst \x1b[1mquoted\x1b[22m line\n",
            "\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22mlazy second line\n",
            "\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22mexplicit third line\n",
            "\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22mlazy fourth line\n\n",
            "outside\n",
        ),
    );
}

#[test]
fn lazy_blockquote_continuations_retain_inline_links() {
    let out = render("> source\nlazy [link](https://example.com/lazy)\n");
    assert!(contains_ansi(&out, "\n\x1b[2m│ \x1b[22mlazy "));
    assert_eq!(link_urls(&out)[0].1, "https://example.com/lazy");
}

#[test]
fn lazy_blockquotes_stop_before_structural_rows_and_preserve_eof_backslashes() {
    let mut processor = with_rules();
    let mut out = Vec::new();
    processor.push(
        "> quote\nlazy continuation\n===\n---\noutside\n> quote again\n- list item\n> quote at EOF\nlazy EOF\\",
        &mut out,
    );
    processor.flush(&mut out);
    assert_ansi(
        &out,
        concat!(
            "\x1b[2m│ \x1b[22mquote\n",
            "\x1b[2m│ \x1b[22mlazy continuation\n",
            "\x1b[2m│ \x1b[22m===\n",
            "outside\n",
            "\x1b[2m│ \x1b[22mquote again\n",
            "\x1b[2m• \x1b[22mlist item\n",
            "\x1b[2m│ \x1b[22mquote at EOF\n",
            "\x1b[2m│ \x1b[22mlazy EOF\\",
        ),
    );
    assert_eq!(rule_count(&out), 1);
}

#[test]
fn lazy_blockquotes_end_before_headings_and_fences() {
    assert_ansi(
        &definition_output(
            "> quote before heading\n# heading\nheading outside\n> quote before fence\n```zig\ncode\n```\nafter fence\n",
        ),
        concat!(
            "\x1b[2m│ \x1b[22mquote before heading\n",
            "\x1b[1m\x1b[4mheading\x1b[24m\x1b[22m\n",
            "heading outside\n",
            "\x1b[2m│ \x1b[22mquote before fence\n",
            "\x1b[2m│ \x1b[22mcode\n",
            "after fence\n",
        ),
    );
}

#[test]
fn flushing_ends_lazy_blockquote_state() {
    let mut processor = plain_processor();
    let mut out = Vec::new();
    processor.push("> quote\n", &mut out);
    processor.flush(&mut out);
    processor.push("next stream\n", &mut out);
    assert_ansi(&out, "\x1b[2m│ \x1b[22mquote\nnext stream\n");
}

#[test]
fn nested_blockquotes_render_every_valid_marker_and_preserve_malformed_inner_syntax() {
    assert_ansi(
        &render("  > > **deep** note\n> > > third depth\n> >\n>> compact\n> >not\n"),
        concat!(
            "  \x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22m\x1b[1mdeep\x1b[22m note\n",
            "\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22mthird depth\n",
            "\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22m\n",
            ">> compact\n",
            "\x1b[2m│ \x1b[22m>not\n",
        ),
    );
}

#[test]
fn nested_blockquote_remains_buffered_until_its_terminating_newline() {
    let mut processor = plain_processor();
    assert!(push(&mut processor, "> > **chunked").is_empty());
    assert_ansi(
        &push(&mut processor, " nested**\n"),
        "\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22m\x1b[1mchunked nested\x1b[22m\n",
    );
}

#[test]
fn blockquote_without_a_marker_separator_stays_literal() {
    assert_ansi(&render(">not a blockquote\n"), ">not a blockquote\n");
}

#[test]
fn code_fence_toggles_block_state() {
    let mut processor = plain_processor();
    let out = push(&mut processor, "```zig\nconst x = 1;\n```\n");
    assert_ansi(&out, "\x1b[2m│ \x1b[22mconst x = 1;\n");
    assert!(processor.code_block.is_none());
}

#[test]
fn code_block_preserves_inline_markers_literally() {
    assert_ansi(
        &render("```\nfoo **bar** baz\n```\n"),
        "\x1b[2m│ \x1b[22mfoo **bar** baz\n",
    );
}

#[test]
fn tilde_code_fence_keeps_literal_code_and_ignores_backtick_fence() {
    let mut processor = plain_processor();
    let out = push(&mut processor, "~~~zig\nconst value = **1**;\n```\n~~~\n");
    assert_ansi(
        &out,
        "\x1b[2m│ \x1b[22mconst value = **1**;\n\x1b[2m│ \x1b[22m```\n",
    );
    assert!(processor.code_block.is_none());
}

#[test]
fn tilde_code_fence_completion_retains_language_and_literal_source() {
    let out = push(
        &mut with_code_blocks(),
        "Before block.\n~~~  Zig example\n  const value = **1**;\n```\n~~~\nAfter block.\n",
    );
    assert_ansi(&out, "Before block.\nAfter block.\n");
    let blocks = code_blocks(&out);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].language, "Zig");
    assert_eq!(blocks[0].code, "  const value = **1**;\n```\n");
    assert!(matches!(out[1], Event::CodeBlock(_)));
}

#[test]
fn unterminated_tilde_code_fence_flushes_its_semantic_payload() {
    let mut processor = with_code_blocks();
    let mut out = Vec::new();
    processor.push("~~~text\nlast line", &mut out);
    processor.flush(&mut out);
    let blocks = code_blocks(&out);
    assert_eq!(blocks[0].language, "text");
    assert_eq!(blocks[0].code, "last line\n");
}

#[test]
fn markdown_completion_normalizes_crlf_across_chunks_before_capturing_code_fences() {
    let mut processor = with_code_blocks();
    let mut out = Vec::new();
    processor.push("Before block.\r\n```  Zig example\r", &mut out);
    processor.push(
        "\n  const value = **1**;\r\n```\r\nAfter block.\r\n",
        &mut out,
    );
    assert_ansi(&out, "Before block.\nAfter block.\n");
    let blocks = code_blocks(&out);
    assert_eq!(blocks[0].language, "Zig");
    assert_eq!(blocks[0].code, "  const value = **1**;\n");
}

#[test]
fn indented_code_completion_deindents_literal_lines_and_reprocesses_its_terminator() {
    let mut processor = with_code_blocks();
    let out = push(
        &mut processor,
        "\n    | literal | pipe |\n    ```zig\n    const value = **1**;\n        nested\nplain text\n",
    );
    let blocks = code_blocks(&out);
    assert_eq!(blocks[0].language, "");
    assert_eq!(
        blocks[0].code,
        "| literal | pipe |\n```zig\nconst value = **1**;\n    nested\n"
    );
    assert_ansi(&out, "\nplain text\n");
    assert!(processor.code_block.is_none());
}

#[test]
fn indented_code_completion_accepts_a_response_leading_tab_and_flushes_at_eof() {
    let mut processor = with_code_blocks();
    let mut out = Vec::new();
    processor.push("\t  tail", &mut out);
    processor.flush(&mut out);
    let blocks = code_blocks(&out);
    assert_eq!(blocks[0].language, "");
    assert_eq!(blocks[0].code, "  tail\n");
    assert!(text_lines(&out).is_empty());
}

#[test]
fn indented_code_requires_a_blank_boundary_and_yields_to_list_and_lazy_quote_content() {
    let out = push(
        &mut with_code_blocks(),
        "paragraph\n    continuation\n\n   three spaces\n\n    - list item\n> quote\n    lazy continuation\n\nafter quote\n",
    );
    assert!(code_blocks(&out).is_empty());
    let text = plain_text(&out);
    assert!(text.contains("    continuation"));
    assert!(text.contains("   three spaces"));
    assert!(contains_ansi(
        &out,
        "\x1b[2m│ \x1b[22m    lazy continuation"
    ));
    assert!(!contains_ansi(&out, "\x1b[2m│ \x1b[22m- list item"));
}

#[test]
fn markdown_completion_flushes_an_unterminated_code_fence() {
    let mut processor = with_code_blocks();
    let mut out = Vec::new();
    processor.push("```text\nlast line", &mut out);
    processor.flush(&mut out);
    let blocks = code_blocks(&out);
    assert_eq!(blocks[0].language, "text");
    assert_eq!(blocks[0].code, "last line\n");
}

#[test]
fn line_intake_buffers_partial_lines_and_preserves_standalone_carriage_returns() {
    let mut processor = plain_processor();
    assert!(push(&mut processor, ">").is_empty());
    assert_ansi(&push(&mut processor, "\n"), "\x1b[2m│ \x1b[22m\n");

    assert!(push(&mut processor, "**bol").is_empty());
    assert_ansi(
        &push(&mut processor, "d** rest\n"),
        "\x1b[1mbold\x1b[22m rest\n",
    );

    assert!(push(&mut processor, "run `zig").is_empty());
    assert_ansi(
        &push(&mut processor, " build` now\n"),
        "run \x1b[38;5;245mzig build\x1b[39m now\n",
    );

    assert_ansi(&push(&mut processor, "left\rright\n"), "left\\x0dright\n");
}

#[test]
fn flush_emits_pending_line_without_newline() {
    assert_ansi(&render_flushed("partial"), "partial");
}

#[test]
fn flush_keeps_an_unmatched_emphasis_opener_literal() {
    assert_ansi(
        &render_flushed("oops **never closed"),
        "oops **never closed",
    );
}

#[test]
fn flush_keeps_an_unpaired_backtick_literal() {
    assert_ansi(&render_flushed("run `zig build"), "run `zig build");
}

#[test]
fn emphasis_pairs_by_the_delimiter_stack_and_unmatched_runs_stay_literal() {
    assert_ansi(
        &render(concat!(
            "*not a list item\n",
            "**bold** then **open\n",
            "~~gone~~ and ~~stays\n",
            "a ` b **bold**\n",
            "**bold with `code` inside** *it*\n",
            "***both*** and **`code`**\n",
            "See *italic** plain\n",
            "text **** here **bold** and ~~~~ then ~~gone~~ and *** end\n",
            "**a __b__ c** and *x _y_ z*\n",
            "foo*bar*baz and snake_case_name and 3 * 5 = 15\n",
        )),
        concat!(
            "*not a list item\n",
            "\x1b[1mbold\x1b[22m then **open\n",
            "\x1b[9mgone\x1b[29m and ~~stays\n",
            "a ` b \x1b[1mbold\x1b[22m\n",
            "\x1b[1mbold with \x1b[38;5;245mcode\x1b[39m inside\x1b[22m \x1b[3mit\x1b[23m\n",
            "\x1b[3m\x1b[1mboth\x1b[22m\x1b[23m and \x1b[1m\x1b[38;5;245mcode\x1b[39m\x1b[22m\n",
            "See \x1b[3mitalic\x1b[23m* plain\n",
            "text **** here \x1b[1mbold\x1b[22m and ~~~~ then \x1b[9mgone\x1b[29m and *** end\n",
            "\x1b[1ma \x1b[1mb\x1b[22m\x1b[1m c\x1b[22m and \x1b[3mx \x1b[3my\x1b[23m\x1b[3m z\x1b[23m\n",
            "foo\x1b[3mbar\x1b[23mbaz and snake_case_name and 3 * 5 = 15\n",
        ),
    );
}

#[test]
fn emphasis_lookahead_agrees_with_links_code_spans_and_bare_urls() {
    let out = render(
        "[use `](https://example.com) **bold** `code`\n[use `](https://example.com) *open `code*`\n",
    );
    assert!(rendered_line(&out, 0).ends_with(&canonical_closed(
        " \x1b[1mbold\x1b[22m \x1b[38;5;245mcode\x1b[39m"
    )));
    assert!(
        rendered_line(&out, 1).ends_with(&canonical_closed(" *open \x1b[38;5;245mcode*\x1b[39m"))
    );

    assert_ansi(
        &render("See _a ``b`c`` d_ end\n"),
        "See \x1b[3ma \x1b[38;5;245mb`c\x1b[39m d\x1b[23m end\n",
    );

    let out = render("https://example.com/`x*y` **bold** `code`\n");
    assert_eq!(link_urls(&out)[0].1, "https://example.com/`x*y`");
    assert!(rendered(&out).ends_with(&canonical(
        " \x1b[1mbold\x1b[22m \x1b[38;5;245mcode\x1b[39m\n"
    )));

    assert_ansi(
        &render("*see https://example.com/a* and `code`\n"),
        "\x1b[3msee \x1b]8;id=fx-0;https://example.com/a\x1b\\\x1b[38;5;75m\x1b[4mhttps://example.com/a\x1b[24m\x1b[39m\x1b]8;;\x1b\\\x1b[23m and \x1b[38;5;245mcode\x1b[39m\n",
    );

    assert_ansi(
        &render("*https://example.com/a*`some code` **bold** `x`\n"),
        "*\x1b]8;id=fx-0;https://example.com/a*`some\x1b\\\x1b[38;5;75m\x1b[4mhttps://example.com/a*`some\x1b[24m\x1b[39m\x1b]8;;\x1b\\ code\x1b[38;5;245m**bold**\x1b[39mx`\n",
    );

    let out = render("text **** https://example.com\n");
    assert!(rendered(&out).starts_with("text **** \x1b]8;"));
}

#[test]
fn emphasis_flanking_reads_neighbouring_code_points_not_bytes() {
    assert_ansi(
        &render("aµ_b_ and xª_y_ and 〱_z_\n"),
        "aµ_b_ and xª_y_ and 〱_z_\n",
    );
    assert_ansi(
        &render("a—_b_ “*q*” 中*強*調 中_強_調 **x**，\n"),
        "a—\x1b[3mb\x1b[23m “\x1b[3mq\x1b[23m” 中\x1b[3m強\x1b[23m調 中_強_調 \x1b[1mx\x1b[22m，\n",
    );
}

#[test]
fn heading_with_unmatched_strong_marker_keeps_it_literal() {
    assert_ansi(
        &render("## 2 ** 8 and **strong** and __also__\n"),
        "\x1b[1m2 ** 8 and strong and also\x1b[22m\n",
    );
}

fn assert_renders_in_linear_time(processor: &mut MarkdownProcessor, line: &str) -> Vec<Event> {
    let started = Instant::now();
    let out = push(processor, line);
    assert!(
        started.elapsed() < LINEAR_TIME_BUDGET,
        "rendering took {:?}",
        started.elapsed()
    );
    out
}

#[test]
fn long_lines_of_escapes_repeated_schemes_and_escaped_openers_render_in_linear_time() {
    let mut processor = plain_processor();
    let size = 120_000;

    let backslashes = format!("{}\n", "\\".repeat(size));
    let out = assert_renders_in_linear_time(&mut processor, &backslashes);
    assert_eq!(plain_text(&out), format!("{}\n", "\\".repeat(size / 2)));

    let schemes = "http://".repeat(size / 7);
    let out = assert_renders_in_linear_time(&mut processor, &format!("{schemes}\n"));
    assert_eq!(plain_text(&out), format!("{schemes}\n"));
    let linked_tail = schemes.len() - MAX_LINK_URL_BYTES / 7 * 7;
    assert_eq!(
        link_urls(&out)
            .into_iter()
            .map(|(_, url)| url)
            .collect::<Vec<_>>(),
        [&schemes[linked_tail..]]
    );

    let brackets = format!("{}](\n", "\\[".repeat(size / 2));
    let out = assert_renders_in_linear_time(&mut processor, &brackets);
    assert_eq!(plain_text(&out), format!("{}](\n", "[".repeat(size / 2)));

    let angles = format!("{}\n", "\\<".repeat(size / 2));
    let out = assert_renders_in_linear_time(&mut processor, &angles);
    assert_eq!(plain_text(&out), format!("{}\n", "<".repeat(size / 2)));
}

#[test]
fn many_footnotes_resolve_in_linear_time() {
    let count = 20_000;
    let mut references = String::new();
    let mut definitions = String::new();
    for index in 0..count {
        let _ = writeln!(references, "[^{index}]");
        let _ = writeln!(definitions, "[^{index}]: note {index}");
    }
    let mut processor = plain_processor();
    let started = Instant::now();
    let mut out = Vec::new();
    processor.push(&references, &mut out);
    processor.push(&definitions, &mut out);
    processor.flush(&mut out);
    assert!(
        started.elapsed() < LINEAR_TIME_BUDGET,
        "rendering took {:?}",
        started.elapsed()
    );
    let text = plain_text(&out);
    assert!(text.starts_with("[1]\n[2]\n"));
    assert!(text.ends_with(&format!("[{count}] note {}\n", count - 1)));
}

#[test]
fn long_lines_of_unmatched_or_unbalanced_delimiters_render_in_linear_time() {
    let mut processor = plain_processor();

    let nested = format!("{}{}\n", "*a ".repeat(32_000), "b* ".repeat(32_000));
    let out = assert_renders_in_linear_time(&mut processor, &nested);
    assert!(rendered(&out).starts_with(&canonical("\x1b[3ma \x1b[3ma ")));

    let residual = format!("{}{}\n", "*a ".repeat(32_000), "_b c__ ".repeat(32_000));
    let out = assert_renders_in_linear_time(&mut processor, &residual);
    assert!(contains_ansi(&out, "\x1b[3mb c\x1b[23m_ "));

    let shapes = [
        ("", "*a _b ~~c ", 40_000, "\n"),
        ("", "a* b_ c~~ ", 40_000, "\n"),
        ("", "*a https://example.com/b _c ~~d ", 20_000, "\n"),
        ("text ", "*", 64 * 1024, "x\n"),
        ("https://example.com ", "*", 64 * 1024, "\n"),
        ("*a", "*", 64 * 1024, "x\n"),
        ("", "[", 64 * 1024, "\n"),
        ("", "![", 32 * 1024, "\n"),
        ("", "[", 64 * 1024, "]\n"),
        ("", "![", 32 * 1024, "]\n"),
        ("", "[^", 32 * 1024, "]\n"),
    ];
    for (prefix, unit, repeat, suffix) in shapes {
        let line = format!("{prefix}{}{suffix}", unit.repeat(repeat));
        assert_renders_in_linear_time(&mut processor, &line);
    }
}

#[test]
fn unmatched_backtick_runs_of_every_length_render_in_linear_time() {
    let mut processor = plain_processor();
    let line: String = (1..=1_400)
        .map(|run| format!("{}x", "`".repeat(run)))
        .chain(["\n".to_owned()])
        .collect();
    let out = assert_renders_in_linear_time(&mut processor, &line);
    assert_eq!(plain_text(&out), line);

    let out = push(&mut processor, "``a`b`` ` c ` ```d``e```\n");
    assert_eq!(plain_text(&out), "a`b c d``e\n");
}

#[test]
fn header_with_inline_markdown() {
    assert_ansi(
        &render("## Using `fx` in CI\n"),
        "\x1b[1mUsing \x1b[38;5;245mfx\x1b[39m in CI\x1b[22m\n",
    );
}

#[test]
fn inline_code_ignores_bold_markers_inside() {
    assert_ansi(
        &render("use `**literal**` here\n"),
        "use \x1b[38;5;245m**literal**\x1b[39m here\n",
    );
}

#[test]
fn pipe_table_header_separator_has_junction_aligned_with_column_pipes() {
    assert_ansi(
        &render_flushed("| Name | Age |\n|------|-----|\n| Ana  | 30  |\n| Bob  | 7   |\n"),
        concat!(
            "\x1b[1mName\x1b[22m │ \x1b[1mAge\x1b[22m\n",
            "─────┼────\n",
            "Ana  │ 30 \n",
            "Bob  │ 7  \n",
        ),
    );
}

#[test]
fn single_column_pipe_table_has_no_junction_on_separator() {
    let text = plain_text(&render_flushed("| Header |\n|--------|\n| data   |\n"));
    assert!(!text.contains('┼'));
    assert!(text.contains("──────"));
}

#[test]
fn pipe_table_with_markdown_in_cells_aligns_dividers_across_all_rows() {
    assert_ansi(
        &render_flushed(
            "| Aspect | Value |\n|--------|-------|\n| **bold** | x |\n| plain    | y |\n",
        ),
        concat!(
            "\x1b[1mAspect\x1b[22m │ \x1b[1mValue\x1b[22m\n",
            "───────┼──────\n",
            "\x1b[1mbold\x1b[22m   │ x    \n",
            "plain  │ y    \n",
        ),
    );
}

#[test]
fn escaped_pipes_stay_inside_table_cells_while_even_backslashes_retain_delimiters() {
    let table =
        parse_table_payload("| Command | Result |\n| --- | --- |\n| printf \\| grep | works |\n");
    assert_eq!(table.column_count, 2);
    assert_eq!(table.rows[1].cells[0][0].text, "printf | grep");
    assert!(!is_pipe_line("literal \\| pipe"));
    assert!(is_pipe_line("literal \\\\| delimiter"));
}

#[test]
fn three_column_pipe_table_aligns_junctions_with_every_column_pipe() {
    let text = plain_text(&render_flushed(
        "| A | B  | C   |\n|---|----|-----|\n| 1 | 22 | 333 |\n",
    ));
    assert!(text.contains("──┼────┼────\n"));
    assert!(text.contains("1 │ 22 │ 333\n"));
}

#[test]
fn pipe_table_without_separator_falls_back_to_plain_lines() {
    assert_ansi(
        &render_flushed("| just one |\n| stray pipes |\n"),
        "| just one |\n| stray pipes |\n",
    );
}

#[test]
fn pipe_table_preceded_by_paragraph_and_followed_by_paragraph() {
    let text = plain_text(&render(
        "Before table.\n| a | b |\n|---|---|\n| 1 | 2 |\nAfter table.\n",
    ));
    assert!(text.starts_with("Before table.\n"));
    assert!(text.ends_with("After table.\n"));
}

#[test]
fn borderless_gfm_table_no_leading_trailing_pipes_is_detected_and_rendered() {
    let out = render_flushed("Name | Age\n-----|-----\nAna  | 30\nBob  | 7\n");
    let text = plain_text(&out);
    assert!(text.contains('│'));
    assert!(text.contains('┼'));
    assert!(contains_ansi(&out, "\x1b[1mName\x1b[22m"));
}

#[test]
fn borderless_separator_with_spaces_around_pipe_is_accepted() {
    let text = plain_text(&render_flushed(concat!(
        "Característica | Vercel | Cloudflare\n",
        "-------------- | ------ | ----------\n",
        "Enfoque principal | Frontend | Edge compute\n",
    )));
    assert!(text.contains('│'));
    assert!(text.contains('┼'));
}

#[test]
fn paragraph_with_single_inline_pipe_falls_back_to_plain_no_separator() {
    let text = plain_text(&render(
        "Use cmd | grep foo to filter.\nThen review the output.\n",
    ));
    assert!(!text.contains('│'));
    assert!(text.contains("Use cmd | grep foo"));
    assert!(text.contains("Then review the output."));
}

#[test]
fn header_with_nested_bold_stays_fully_bold_across_the_whole_line() {
    assert_ansi(
        &render("## **Directory** Structure\n"),
        "\x1b[1mDirectory Structure\x1b[22m\n",
    );
}

#[test]
fn header_keeps_italic_and_code_inline() {
    assert_ansi(
        &render("# *note* about `fx`\n"),
        "\x1b[1m\x1b[4m\x1b[3mnote\x1b[23m about \x1b[38;5;245mfx\x1b[39m\x1b[24m\x1b[22m\n",
    );
}

#[test]
fn strikethrough_wraps_with_ansi() {
    assert_ansi(
        &render("was ~~wrong~~ now right\n"),
        "was \x1b[9mwrong\x1b[29m now right\n",
    );
}

#[test]
fn stray_tildes_between_spaces_stay_literal() {
    assert_ansi(&render("path is ~~ /tmp\n"), "path is ~~ /tmp\n");
}

#[test]
fn horizontal_rule_with_dashes_renders_dim_line() {
    let out = render("---\n");
    let lines = text_lines(&out);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].spans.len(), 1);
    assert!(lines[0].spans[0].style.has(Attr::Dim));
    assert_eq!(
        lines[0].spans[0].text,
        TABLE_HORIZ.repeat(HORIZONTAL_RULE_WIDTH)
    );
}

#[test]
fn thematic_rule_completion_handles_newline_and_eof_without_matching_h6_content() {
    let mut processor = with_rules();
    let out = push(&mut processor, "---\n");
    assert_eq!(rule_count(&out), 1);
    assert!(text_lines(&out).is_empty());

    let mut out = Vec::new();
    processor.push("___", &mut out);
    processor.flush(&mut out);
    assert_eq!(rule_count(&out), 1);
    assert!(text_lines(&out).is_empty());

    let heading = format!("###### {}\n", TABLE_HORIZ.repeat(HORIZONTAL_RULE_WIDTH));
    let out = push(&mut processor, &heading);
    assert_eq!(rule_count(&out), 0);
    assert!(!text_lines(&out).is_empty());
}

#[test]
fn horizontal_rule_with_asterisks_and_spaces_also_renders() {
    let text = plain_text(&render("* * *\n"));
    assert!(text.contains(TABLE_HORIZ));
    assert!(!text.contains('*'));
}

#[test]
fn hyphen_space_line_is_a_list_not_a_rule() {
    let text = plain_text(&render("- item\n"));
    assert!(text.contains('•'));
    assert!(!text.contains('─'));
}

#[test]
fn nested_unordered_list_preserves_indent() {
    assert_ansi(
        &render("  - nested item\n"),
        "  \x1b[2m• \x1b[22mnested item\n",
    );
}

#[test]
fn nested_ordered_list_preserves_indent() {
    assert_ansi(
        &render("    1. sub-step\n"),
        "    \x1b[2m1.\x1b[22m sub-step\n",
    );
}

#[test]
fn right_aligned_gfm_column_pads_cell_on_the_left() {
    let text = plain_text(&render_flushed(
        "| Name | Score |\n|:-----|------:|\n| Ana  |     5 |\n",
    ));
    assert!(text.contains("    5\n"));
}

#[test]
fn center_aligned_gfm_column_pads_both_sides() {
    let text = plain_text(&render_flushed(
        "| Name | Note |\n|------|:----:|\n| Ana  | ok   |\n",
    ));
    assert!(text.contains(" ok \n"));
}

#[test]
fn pipe_table_parsing_retains_styled_cells_alignment_and_ragged_rows() {
    let table = parse_table_payload(concat!(
        "| Name | State | Count |\n",
        "|:-----|:-----:|------:|\n",
        "| **api** | ready | 7 | extra |\n",
        "| worker | |\n",
    ));
    assert_eq!(table.column_count, 3);
    assert_eq!(table.rows.len(), 3);
    assert_eq!(table.alignments[1], TableColumnAlign::Center);
    assert_eq!(table.alignments[2], TableColumnAlign::Right);
    let api = &table.rows[1].cells[0];
    assert_eq!(api.len(), 1);
    assert_eq!(api[0].text, "api");
    assert!(api[0].style.has(Attr::Bold));
    assert_eq!(table.rows[1].cells.len(), 3);
    assert_eq!(table.rows[2].cells.len(), 2);
}

#[test]
fn pipe_table_inside_code_block_stays_literal() {
    let text = plain_text(&render(
        "```\n| not | a |\n|-----|---|\n| table | really |\n```\n",
    ));
    assert!(text.contains("| not | a |"));
    assert!(text.contains("| table | really |"));
}

#[test]
fn streamed_deltas_render_exactly_like_one_complete_chunk() {
    let inputs = [
        "# Title\n\nSome **bold** and `code` with [a link](https://example.com).\n- item one\n- [x] done\n> quote\nlazy\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```zig\nconst x = 1;\n```\nfootnote[^n]\n[^n]: body\ntail",
        "Term\n: definition\n\n***\nSetext\n===\n1. one\n   2. two\n\n    code\nafter",
    ];
    for completions in [Completions::default(), Completions::ALL] {
        for input in inputs {
            let mut whole = MarkdownProcessor::with_completions(completions);
            let mut expected = Vec::new();
            whole.push(input, &mut expected);
            whole.flush(&mut expected);

            let mut streamed = MarkdownProcessor::with_completions(completions);
            let mut actual = Vec::new();
            for character in input.chars() {
                streamed.push(character.encode_utf8(&mut [0; 4]), &mut actual);
            }
            streamed.flush(&mut actual);
            assert_eq!(
                rendered(&actual),
                rendered(&expected),
                "{completions:?} {input:?}"
            );
            assert_eq!(actual.len(), expected.len());
        }
    }
}

#[test]
fn each_delta_only_emits_lines_it_completes() {
    let mut processor = plain_processor();
    assert!(push(&mut processor, "first line").is_empty());
    let out = push(&mut processor, " continues\nsecond");
    assert_ansi(&out, "first line continues\n");
    let out = push(&mut processor, "\n");
    assert_ansi(&out, "second\n");
}

#[test]
fn lines_carry_hanging_indents_for_wrapped_continuations() {
    let out = render(concat!(
        "plain\n",
        "  indented prose\n",
        "- bullet\n",
        "  * [x] nested task\n",
        "12. ordered\n",
        "3) [ ] ordered task\n",
        "\t- tab indented\n",
        "> > quoted\n",
        "  > - quoted item\n",
    ));
    assert_eq!(
        hangs(&out),
        [
            Hang::None,
            Hang::Indent(2),
            Hang::Indent(2),
            Hang::Indent(4),
            Hang::Indent(4),
            Hang::Indent(5),
            Hang::None,
            Hang::Quote {
                indent: 0,
                depth: 2
            },
            Hang::Quote {
                indent: 2,
                depth: 1
            },
        ]
    );

    let out = definition_output("Term\n: body\n");
    assert_eq!(hangs(&out), [Hang::None, Hang::Indent(2)]);

    let out = render_flushed("note[^a]\n[^a]: first\n  second\n");
    assert_eq!(
        hangs(&out),
        [Hang::None, Hang::None, Hang::Indent(4), Hang::Indent(4)]
    );

    let out = render("```\ncode\n```\n");
    assert_eq!(
        hangs(&out),
        [Hang::Quote {
            indent: 0,
            depth: 1
        }]
    );
}

#[test]
fn rendered_payload_helpers_project_lines() {
    let lines = render_code_block_payload(&CodeBlockPayload {
        language: "zig".to_owned(),
        code: "a\nb\n".to_owned(),
    });
    assert_lines(&lines, "\x1b[2m│ \x1b[22ma\n\x1b[2m│ \x1b[22mb\n");

    let table = parse_table_payload("| Name |\n|---|\n| api |\n");
    assert_lines(
        &render_table_payload(&table),
        "\x1b[1mName\x1b[22m\n────\napi \n",
    );
    let header = crate::table_header_cell(&table.rows[1].cells[0]);
    assert!(header[0].style.has(Attr::Bold));
    assert_eq!(header[0].style.slot, None);
    assert_eq!(
        text_lines(&render("`x`\n"))[0].spans[0].style.slot,
        Some(Slot::InlineCode)
    );
}

fn has_terminal_control(text: &str) -> bool {
    matches!(escape_terminal_controls(text), std::borrow::Cow::Owned(_))
}

fn assert_terminal_safe_line(line: &Line) {
    for span in &line.spans {
        assert!(!has_terminal_control(&span.text), "{:?}", span.text);
        if let Some(link) = &span.link {
            assert!(
                link.url.chars().all(ofx_text::is_terminal_safe_char),
                "{:?}",
                link.url
            );
        }
    }
}

fn displayed_lines(events: &[Event]) -> Vec<Line> {
    let mut lines = Vec::new();
    for event in events {
        match event {
            Event::Line(line) => lines.push(line.clone()),
            Event::Table(table) => lines.extend(render_table_payload(table)),
            Event::CodeBlock(block) => lines.extend(render_code_block_payload(block)),
            Event::ThematicRule => {}
        }
    }
    lines
}

fn assert_terminal_safe(events: &[Event]) {
    for line in displayed_lines(events) {
        assert_terminal_safe_line(&line);
    }
    for event in events {
        if let Event::CodeBlock(block) = event {
            assert!(
                !has_terminal_control(&block.language),
                "{:?}",
                block.language
            );
        }
    }
}

fn displayed_text(events: &[Event]) -> String {
    displayed_lines(events)
        .iter()
        .map(|line| line.text() + "\n")
        .collect()
}

#[test]
fn link_targets_reject_c1_controls_bidi_overrides_and_invisible_code_points() {
    for input in [
        "[x](<http://a\u{9c}\u{9b}2J>)\n",
        "<http://a\u{9c}\u{9b}2J>\n",
        "[x](http://e.example/\u{202e}gpj.exe)\n",
        "`https://a.b/\u{9c}x`\n",
        "see https://e.example/\u{2066}x now\n",
        "[x](https://e.example/a\u{200b}b)\n",
        "![x](https://e.example/\u{85}.png)\n",
    ] {
        let out = render(input);
        assert!(!has_link(&out), "{input:?}");
        assert_terminal_safe(&out);
    }
}

#[test]
fn terminal_controls_reach_every_rendered_span_escaped_exactly_once() {
    let input = "plain \x1b]0;title\x07 \u{9b}2J\u{85} \u{202e}rev\n\
                 # head \x1b[5m\n\
                 `\x1b[31m`\n\
                 | \x1b[31mab | c |\n|---|---|\n| d\u{7} | e |\n\n\
                 ```\x1b[31mrust\x07\ncode \x1b[2J\n```\n";
    for completions in [Completions::default(), Completions::ALL] {
        let mut processor = MarkdownProcessor::with_completions(completions);
        let mut out = Vec::new();
        processor.push(input, &mut out);
        processor.flush(&mut out);
        assert_terminal_safe(&out);
        let displayed = displayed_text(&out);
        for escaped in [
            "plain \\x1b]0;title\\x07 \\u{009b}2J\\u{0085} \\u{202e}rev",
            "head \\x1b[5m",
            "\\x1b[31m",
            "\\x1b[31mab",
            "d\\x07",
            "code \\x1b[2J",
        ] {
            assert!(displayed.contains(escaped), "{escaped:?} in {displayed:?}");
        }
        assert!(!displayed.contains("\\\\x"), "{displayed:?}");
    }
    let blocks = code_blocks(&{
        let mut out = Vec::new();
        with_code_blocks().push(input, &mut out);
        out
    });
    assert_eq!(blocks[0].language, "\\x1b[31mrust\\x07");
    assert_eq!(blocks[0].code, "code \x1b[2J\n");
}

#[test]
fn numeric_entities_for_bidi_and_invisible_code_points_stay_literal() {
    let input = "&#x202E;gnp.exe &#8238; &#x2066; &#x200B; &#xAD; &#x85; &#x2028;\n";
    let out = render(input);
    assert_eq!(plain_text(&out), input);
    assert_terminal_safe(&out);
}

fn table_payloads(events: &[Event]) -> Vec<TablePayload> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Table(table) => Some(table.clone()),
            _ => None,
        })
        .collect()
}

fn displayed_bytes(events: &[Event]) -> usize {
    displayed_lines(events)
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.text.len())
        .sum()
}

#[test]
fn pipe_tables_take_their_column_count_from_the_header_row() {
    let mut processor = MarkdownProcessor::with_completions(Completions::ALL);
    let out = push(
        &mut processor,
        "| a | b |\n|---|---|\n| 1 | 2 | 3 |\n| 4 |\n\n",
    );
    let tables = table_payloads(&out);
    assert_eq!(tables[0].column_count, 2);
    assert_eq!(
        tables[0]
            .rows
            .iter()
            .map(|row| row.cells.len())
            .collect::<Vec<_>>(),
        [2, 2, 1]
    );
    assert_eq!(displayed_text(&out), "a │ b\n──┼──\n1 │ 2\n4 │  \n\n");
}

#[test]
fn pipe_table_output_stays_proportional_to_its_input() {
    for header in ["|".repeat(1_000), format!("|{}|", "x".repeat(1_000))] {
        let mut input = format!("{header}\n|-|\n");
        while input.len() < 8 * 1024 {
            input.push_str("|\n");
        }
        for completions in [Completions::default(), Completions::ALL] {
            let mut processor = MarkdownProcessor::with_completions(completions);
            let mut out = Vec::new();
            processor.push(&input, &mut out);
            processor.flush(&mut out);
            let displayed = displayed_bytes(&out);
            assert!(
                displayed <= 16 * input.len(),
                "{displayed} bytes from {}",
                input.len()
            );
        }
    }
}

#[test]
fn rendering_a_table_payload_with_more_cells_than_columns_widens_the_table() {
    let cell = |text: &str| {
        vec![Span {
            text: text.to_owned(),
            ..Span::default()
        }]
    };
    let table = TablePayload {
        rows: vec![
            TableRow {
                cells: vec![cell("a"), cell("bb")],
            },
            TableRow {
                cells: vec![cell("c"), cell("d"), cell("e")],
            },
        ],
        alignments: Vec::new(),
        column_count: 1,
    };
    let text: Vec<String> = render_table_payload(&table)
        .iter()
        .map(Line::text)
        .collect();
    assert_eq!(text, ["a │ bb │  ", "──┼────┼──", "c │ d  │ e"]);
}

#[test]
fn table_widths_measure_escaped_controls_as_displayed() {
    let raw = "| \x1b[31mab | c |\n|---|---|\n| d | e |\n";
    let literal = "| \\x1b[31mab | c |\n|---|---|\n| d | e |\n";
    for completions in [Completions::default(), Completions::ALL] {
        let render = |input: &str| {
            let mut processor = MarkdownProcessor::with_completions(completions);
            let mut out = Vec::new();
            processor.push(input, &mut out);
            processor.flush(&mut out);
            displayed_text(&out)
        };
        assert_eq!(render(raw), render(literal));
        assert_eq!(
            render(raw),
            "\\x1b[31mab │ c\n───────────┼──\nd          │ e\n"
        );
    }
}

#[test]
fn flush_leaves_events_from_earlier_pushes_untouched() {
    for input in [
        "Done [^a]\n\n\n\n[^a]: note\n",
        "Done [^a]\n\n[^a]: note\n\ntail",
        "text\n\n\n",
    ] {
        let mut reused = MarkdownProcessor::with_completions(Completions::ALL);
        let mut transcript = Vec::new();
        reused.push(input, &mut transcript);
        let pushed = transcript.clone();
        reused.flush(&mut transcript);
        assert_eq!(transcript[..pushed.len()], pushed[..], "{input:?}");

        let mut separate = MarkdownProcessor::with_completions(Completions::ALL);
        let mut expected = Vec::new();
        separate.push(input, &mut expected);
        let mut flushed = Vec::new();
        separate.flush(&mut flushed);
        expected.extend(flushed);
        assert_eq!(plain_text(&transcript), plain_text(&expected), "{input:?}");
    }
}

#[test]
fn footnote_separator_follows_a_code_block_completed_by_flush() {
    let mut processor = MarkdownProcessor::with_completions(Completions::ALL);
    let mut out = Vec::new();
    processor.push("text[^a]\n\n[^a]: note\n\n```\ncode", &mut out);
    processor.flush(&mut out);
    let block = out
        .iter()
        .position(|event| matches!(event, Event::CodeBlock(_)))
        .expect("code block");
    assert_eq!(plain_text(&out[block + 1..]), "\n[1] note\n");
}
