use super::*;

#[test]
fn without_a_runtime_the_section_says_there_are_no_servers() {
    let section = McpServers::new(None, true).section();
    assert_eq!(
        section.text,
        include_str!("../../../../parity/goldens/mcp_servers_section.txt")
    );
    assert_eq!(section.change_notice, None);
    assert_eq!(section.notice, None);
}

fn summary(name: &str, availability: Availability, tool_count: Option<usize>) -> ServerSummary {
    ServerSummary {
        name: name.to_owned(),
        availability,
        tool_count,
    }
}

#[test]
fn a_change_is_reported_once_after_a_settled_listing() {
    let servers = McpServers::new(None, true);
    let ready = [summary("docs", Availability::Ready, Some(2))];
    assert_eq!(servers.change_notice(&ready), None);
    assert_eq!(servers.change_notice(&ready), None);
    let discovering = [summary("docs", Availability::Discovering, None)];
    assert_eq!(servers.change_notice(&discovering), None);
    let failed = [summary("docs", Availability::Failed, None)];
    let notice = servers.change_notice(&failed).unwrap();
    assert!(notice.contains("  docs: ready -> failed\n"), "{notice}");
    assert_eq!(servers.change_notice(&failed), None);
    let quiet = McpServers::new(None, false);
    assert_eq!(quiet.change_notice(&ready), None);
    assert_eq!(quiet.change_notice(&failed), None);
}
