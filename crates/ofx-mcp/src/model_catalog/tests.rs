use super::*;

const SECTION_GOLDEN: &str = include_str!("../../../../parity/goldens/mcp_servers_section.txt");
const CHANGE_GOLDEN: &str =
    include_str!("../../../../parity/goldens/mcp_servers_change_notice.txt");

fn server(name: &str, availability: Availability, tool_count: Option<usize>) -> ServerSummary {
    ServerSummary {
        name: name.to_owned(),
        availability,
        tool_count,
        always_loaded: false,
    }
}

fn baseline(name: &str, availability: Availability) -> BaselineEntry {
    BaselineEntry {
        name: name.to_owned(),
        availability,
    }
}

fn notice_with(lines: &str) -> String {
    let (header, footer) = CHANGE_GOLDEN.split_once('\n').unwrap();
    format!("{header}\n{lines}{footer}")
}

#[test]
fn an_empty_catalog_says_so_as_upstream_writes_it() {
    let section = render_model_catalog(&[]);
    assert_eq!(section.text, SECTION_GOLDEN);
    assert!(section.text.contains("<none />"));
    assert_eq!(section.notice, None);
    assert_eq!(render_with_limit(&[], 10).text, "");
}

#[test]
fn servers_are_listed_in_name_order_without_tool_metadata() {
    let section = render_model_catalog(&[
        server("zeta", Availability::Failed, None),
        server("alpha", Availability::Ready, Some(2)),
    ]);
    let (header, footer) = SECTION_GOLDEN.split_once("  <none />\n").unwrap();
    assert_eq!(
        section.text,
        format!(
            "{header}  <server name=\"alpha\" state=\"ready\" tools=\"2\" />\n  <server name=\"zeta\" state=\"failed\" />\n{footer}"
        )
    );
    assert_eq!(section.notice, None);
    let bytes = render_model_catalog(&[
        server("b", Availability::Ready, Some(1)),
        server("B", Availability::Ready, Some(1)),
        server("a", Availability::Ready, Some(1)),
    ])
    .text;
    let order: Vec<usize> = ["\"B\"", "\"a\"", "\"b\""]
        .iter()
        .map(|name| bytes.find(name).unwrap())
        .collect();
    assert!(order.is_sorted(), "{bytes}");
}

#[test]
fn ready_always_loaded_servers_with_tools_are_marked_loaded() {
    let loaded = |name: &str, availability, tool_count| ServerSummary {
        always_loaded: true,
        ..server(name, availability, tool_count)
    };
    let text = render_model_catalog(&[
        loaded("browser", Availability::Ready, Some(3)),
        loaded("empty", Availability::Ready, Some(0)),
        server("lazy", Availability::Ready, Some(1)),
        loaded("down", Availability::Failed, None),
    ])
    .text;
    for line in [
        "<server name=\"browser\" state=\"ready\" tools=\"3\" loaded=\"true\" />",
        "<server name=\"empty\" state=\"ready\" tools=\"0\" />",
        "<server name=\"lazy\" state=\"ready\" tools=\"1\" />",
        "<server name=\"down\" state=\"failed\" />",
    ] {
        assert!(text.contains(line), "{text}");
    }
}

#[test]
fn names_are_encoded_and_omissions_stay_within_the_budget_without_naming_them() {
    let servers = [
        server("alpha", Availability::Ready, Some(1)),
        server("bravo", Availability::Ready, Some(2)),
        server("hidden<&\"", Availability::Failed, None),
    ];
    let section = render_with_limit(&servers, 280);
    assert!(section.text.len() <= 280);
    let notice = section.notice.unwrap();
    assert_eq!(
        notice,
        "[context] omitted 3 MCP servers from the model catalog because the fixed 280-byte budget was reached"
    );
    assert!(!section.text.contains("hidden"));
    let limit = HEADER.len() + FOOTER.len() + 150;
    let roomy = render_with_limit(&servers, limit);
    assert!(
        roomy.text.contains("<server name=\"alpha\""),
        "{}",
        roomy.text
    );
    assert!(
        roomy
            .text
            .contains("  <catalog_truncated omitted_count=\"1\" />\n</mcp_servers>\n"),
        "{}",
        roomy.text
    );
    assert!(!roomy.text.contains("hidden"));
    assert_eq!(
        roomy.notice,
        Some(format!(
            "[context] omitted 1 MCP server from the model catalog because the fixed {limit}-byte budget was reached"
        ))
    );
    let encoded = render_model_catalog(&[server("x<&\"y", Availability::Failed, None)]).text;
    assert!(encoded.contains("<server name=\"x&lt;&amp;&quot;y\" state=\"failed\" />"));
}

#[test]
fn availability_follows_the_classified_connection() {
    for (connection, expected) in [
        (ConnectionState::Ready, Availability::Ready),
        (ConnectionState::Connecting, Availability::Discovering),
        (ConnectionState::Disabled, Availability::Disabled),
        (ConnectionState::Failed, Availability::Failed),
        (ConnectionState::Disconnected, Availability::Unavailable),
    ] {
        assert_eq!(classify_availability(connection), expected);
    }
}

#[test]
fn an_unchanged_availability_renders_no_notice() {
    assert_eq!(
        render_change_notice(
            &[baseline("linear", Availability::Ready)],
            &[server("linear", Availability::Ready, Some(12))],
        ),
        None
    );
}

#[test]
fn changes_name_each_transition_addition_and_removal() {
    assert_eq!(
        render_change_notice(
            &[baseline("linear", Availability::Failed)],
            &[server("linear", Availability::Ready, Some(74))],
        ),
        Some(notice_with("  linear: failed -> ready (74 tools)\n"))
    );
    assert_eq!(
        render_change_notice(
            &[baseline("linear", Availability::Ready)],
            &[server("linear", Availability::Failed, None)],
        ),
        Some(notice_with("  linear: ready -> failed\n"))
    );
    assert_eq!(
        render_change_notice(
            &[baseline("slack", Availability::Ready)],
            &[server("notion", Availability::Failed, None)],
        ),
        Some(notice_with("  notion: added (failed)\n  slack: removed\n"))
    );
}

#[test]
fn the_notice_lists_eight_changes_and_counts_the_rest() {
    let names: Vec<String> = (0..10).map(|index| format!("server<{index}>")).collect();
    let previous: Vec<BaselineEntry> = names
        .iter()
        .map(|name| baseline(name, Availability::Failed))
        .collect();
    let current: Vec<ServerSummary> = names
        .iter()
        .map(|name| server(name, Availability::Ready, None))
        .collect();
    let notice = render_change_notice(&previous, &current).unwrap();
    let mut lines = String::new();
    for index in 0..8 {
        let _ = writeln!(lines, "  server&lt;{index}&gt;: failed -> ready");
    }
    assert_eq!(
        notice,
        notice_with(&format!("{lines}  and 2 more changes\n"))
    );
    let mut nine = current.clone();
    nine.truncate(9);
    assert!(
        render_change_notice(&previous[..9], &nine)
            .unwrap()
            .contains("  and 1 more change\n")
    );
}
