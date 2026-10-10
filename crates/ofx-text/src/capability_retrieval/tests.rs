use super::*;

fn query(raw: &str) -> PreparedQuery {
    PreparedQuery::prepare(raw.to_owned()).unwrap()
}

fn request(query: &PreparedQuery) -> Request<'_> {
    Request {
        query,
        server: None,
    }
}

fn scoped<'a>(query: &'a PreparedQuery, server: &'a str) -> Request<'a> {
    Request {
        query,
        server: Some(server),
    }
}

#[test]
fn corpus_relevance_prefers_identities_and_rejects_one_weak_generic_hit() {
    let documents = [
        Document {
            identities: ["prompt-master", ""],
            stable_key: "skill:prompt-master",
            primary: ["prompt-master", "", "", ""],
            secondary: ["Write prompts for tools", "", ""],
            ..Document::default()
        },
        Document {
            identities: ["mcp_datadog_list_monitors", "list_monitors"],
            stable_key: "mcp:datadog:list_monitors",
            primary: ["datadog", "list_monitors", "mcp_datadog_list_monitors", ""],
            secondary: ["List Datadog monitors and incidents", "", ""],
            ..Document::default()
        },
        Document {
            identities: ["mcp_other_list_tools", "list_tools"],
            stable_key: "mcp:other:list_tools",
            primary: ["other", "list_tools", "mcp_other_list_tools", ""],
            secondary: ["Return tools from a server", "", ""],
            ..Document::default()
        },
    ];
    let query = query("datadog monitor incidents");
    let page = retrieve(request(&query), Domain::Mcp, &documents);
    assert_eq!(page.total_matches, 1);
    assert_eq!(page.matches, [1]);
}

#[test]
fn relevance_rejects_isolated_generic_primary_and_short_secondary_evidence() {
    let documents = [
        Document {
            identities: ["strategy-website", ""],
            stable_key: "skill:strategy-website",
            primary: ["strategy-website", "", "", ""],
            secondary: [
                "Website content, conversion optimization, and call-to-action guidance. Triggers on landing-page requests.",
                "",
                "",
            ],
            ..Document::default()
        },
        Document {
            identities: ["mcp_context7_query-docs", ""],
            stable_key: "mcp:context7:query-docs",
            primary: ["context7", "query-docs", "", ""],
            secondary: ["Call this tool on every documentation request.", "", ""],
            ..Document::default()
        },
    ];
    for raw in [
        "query production monitoring alerts and open incidents datadog pagerduty grafana sentry status page",
        "incident management on-call alerts",
    ] {
        let query = query(raw);
        let page = retrieve(request(&query), Domain::Skill, &documents);
        assert_eq!(page.total_matches, 0);
        assert!(page.matches.is_empty());
    }
}

#[test]
fn exact_server_identity_bypasses_the_two_hit_relevance_floor() {
    let documents = [
        Document {
            identities: ["mcp_context7_query-docs", ""],
            stable_key: "mcp:context7:query-docs",
            primary: ["context7", "query-docs", "", ""],
            ..Document::default()
        },
        Document {
            identities: ["mcp_datadog_list-monitors", ""],
            stable_key: "mcp:datadog:list-monitors",
            primary: ["datadog", "list-monitors", "", ""],
            ..Document::default()
        },
    ];
    let query = query("datadog");
    let page = retrieve(request(&query), Domain::Mcp, &documents);
    assert_eq!(page.total_matches, 1);
    assert_eq!(page.matches, [1]);
}

#[test]
fn rare_short_technical_terms_remain_secondary_evidence() {
    let documents: Vec<_> = (0..RARE_SHORT_TOKEN_CATALOG_DIVISOR)
        .map(|index| Document {
            identities: ["generic-helper", ""],
            stable_key: if index == 0 {
                "skill:cloud-helper"
            } else {
                "skill:generic-helper"
            },
            primary: [
                if index == 0 {
                    "cloud-helper"
                } else {
                    "generic-helper"
                },
                "",
                "",
                "",
            ],
            secondary: [
                if index == 0 {
                    "AWS deployment guidance"
                } else {
                    "General workflow guidance"
                },
                "",
                "",
            ],
            ..Document::default()
        })
        .collect();
    let query = query("aws deployment");
    let page = retrieve(request(&query), Domain::Skill, &documents);
    assert_eq!(page.total_matches, 1);
    assert_eq!(page.matches, [0]);
}

#[test]
fn intent_relevance_rejects_corpus_wide_procedural_description_terms() {
    let documents = [
        Document {
            identities: ["mcp_context7_query-docs", ""],
            stable_key: "mcp:context7:query-docs",
            primary: ["context7", "query-docs", "", ""],
            secondary: ["Query and list documentation", "", ""],
            ..Document::default()
        },
        Document {
            identities: ["mcp_context7_resolve-library-id", ""],
            stable_key: "mcp:context7:resolve-library-id",
            primary: ["context7", "resolve-library-id", "", ""],
            secondary: ["Query and list documentation", "", ""],
            ..Document::default()
        },
    ];
    let query = query("query list production monitors");
    let page = retrieve(request(&query), Domain::Mcp, &documents);
    assert_eq!(page.total_matches, 0);
    assert!(page.matches.is_empty());
}

#[test]
fn a_server_scope_counts_catalog_wide_description_terms_as_evidence() {
    let documents = [
        Document {
            identities: ["mcp_datadog_alpha", ""],
            stable_key: "mcp_datadog_alpha",
            primary: ["datadog", "alpha", "mcp_datadog_alpha", ""],
            secondary: ["Read production monitors", "", ""],
            ..Document::default()
        },
        Document {
            identities: ["mcp_datadog_beta", ""],
            stable_key: "mcp_datadog_beta",
            primary: ["datadog", "beta", "mcp_datadog_beta", ""],
            secondary: ["Read production monitors", "", ""],
            ..Document::default()
        },
    ];
    let query = query("production monitors");
    assert_eq!(
        retrieve(request(&query), Domain::Mcp, &documents).total_matches,
        0
    );
    let page = retrieve(scoped(&query, "datadog"), Domain::Mcp, &documents);
    assert_eq!(page.total_matches, 2);
    assert_eq!(page.matches, [0, 1]);
}

#[test]
fn tags_count_as_primary_fields() {
    let tags = ["mcp".to_owned(), "monitors".to_owned()];
    let documents = [
        Document {
            identities: ["mcp_datadog_list", ""],
            stable_key: "mcp_datadog_list",
            primary: ["datadog", "list", "mcp_datadog_list", ""],
            primary_extra: &tags,
            ..Document::default()
        },
        Document {
            identities: ["mcp_other_list", ""],
            stable_key: "mcp_other_list",
            primary: ["other", "list", "mcp_other_list", ""],
            ..Document::default()
        },
    ];
    let query = query("monitors");
    let page = retrieve(request(&query), Domain::Mcp, &documents);
    assert_eq!(page.matches, [0]);
}

#[test]
fn an_inventory_lists_every_tool_in_stable_order_five_at_a_time() {
    let names: Vec<_> = (0..28)
        .map(|index| format!("mcp_datadog_tool_{index:0>2}"))
        .collect();
    let documents: Vec<_> = names
        .iter()
        .rev()
        .map(|name| Document {
            identities: [name, ""],
            stable_key: name,
            primary: ["datadog", name, "", ""],
            ..Document::default()
        })
        .collect();
    let query = query("");
    let page = retrieve(scoped(&query, "datadog"), Domain::Mcp, &documents);
    assert_eq!(page.total_matches, 28);
    assert_eq!(page.matches, [27, 26, 25, 24, 23]);
    assert!(page.cursor_after(5).unwrap().ends_with(":5"));
    assert!(page.cursor_after(27).unwrap().starts_with("c1:m:"));
    assert_eq!(page.cursor_after(28), None);
}

#[test]
fn the_cursor_is_bound_to_the_request_and_the_catalog() {
    let mut documents = [
        Document {
            identities: ["mcp_datadog_monitors", ""],
            stable_key: "mcp:datadog:monitors",
            primary: ["datadog", "monitors", "", ""],
            secondary: ["Monitor incidents", "", ""],
            ..Document::default()
        },
        Document {
            identities: ["mcp_datadog_incidents", ""],
            stable_key: "mcp:datadog:incidents",
            primary: ["datadog", "incidents", "", ""],
            secondary: ["Monitor incidents", "", ""],
            ..Document::default()
        },
    ];
    let original = query("monitor incidents");
    let reordered = query("incidents monitor");
    let parts = |query: &PreparedQuery, server: &str, documents: &[Document<'_>]| {
        retrieve(scoped(query, server), Domain::Mcp, documents)
            .cursor_after(1)
            .unwrap()
            .split(':')
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let first = parts(&original, "datadog", &documents);
    assert_eq!(first[..2], ["c1", "m"]);
    assert_eq!(first[4], "1");
    assert_eq!(parts(&original, "datadog", &documents), first);
    let other_server = parts(&original, "other", &documents);
    assert_eq!(other_server[2], first[2]);
    assert_ne!(other_server[3], first[3]);
    let other_query = parts(&reordered, "datadog", &documents);
    assert_eq!(other_query[2], first[2]);
    assert_ne!(other_query[3], first[3]);
    documents[1].secondary[0] = "Monitor incidents, changed";
    let changed_catalog = parts(&original, "datadog", &documents);
    assert_ne!(changed_catalog[2], first[2]);
    assert_eq!(changed_catalog[3], first[3]);
}

#[test]
fn large_catalog_retrieval_remains_bounded_to_the_requested_page() {
    let documents: Vec<_> = (0..10_000)
        .map(|index| Document {
            identities: ["monitor", ""],
            stable_key: if index % 2 == 0 {
                "monitor-even"
            } else {
                "monitor-odd"
            },
            primary: ["datadog", "monitor", "", ""],
            secondary: ["Read monitor incidents", "", ""],
            ..Document::default()
        })
        .collect();
    let query = query("monitor incidents");
    let page = retrieve(scoped(&query, "datadog"), Domain::Mcp, &documents);
    assert_eq!(page.total_matches, 10_000);
    assert_eq!(page.matches.len(), PAGE_LIMIT);
    assert!(page.cursor_after(page.matches.len()).unwrap().len() <= 160);
}

#[test]
fn stable_keys_break_identical_rank_ties() {
    let documents = [
        Document {
            identities: ["review", ""],
            stable_key: "/B",
            primary: ["review", "", "", ""],
            secondary: ["test", "", ""],
            ..Document::default()
        },
        Document {
            identities: ["review", ""],
            stable_key: "/a",
            primary: ["review", "", "", ""],
            secondary: ["test", "", ""],
            ..Document::default()
        },
    ];
    for raw in ["review", "Review review"] {
        let query = query(raw);
        assert_eq!(
            retrieve(request(&query), Domain::Skill, &documents).matches,
            [1, 0]
        );
    }
}

#[test]
fn more_than_255_distinct_evidence_tokens_do_not_overflow() {
    let raw = (0..256)
        .map(|i| format!("term{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(raw.len() <= 4096);
    let query = query(&raw);
    assert_eq!(query.tokens().len(), 256);
    let documents = [
        Document {
            identities: ["selected", ""],
            stable_key: "/a",
            primary: ["selected", "", "", ""],
            secondary: [&raw, "", ""],
            ..Document::default()
        },
        Document {
            identities: ["other", ""],
            stable_key: "/b",
            primary: ["other", "", "", ""],
            ..Document::default()
        },
    ];
    assert_eq!(
        retrieve(request(&query), Domain::Skill, &documents).matches,
        [0]
    );
}
