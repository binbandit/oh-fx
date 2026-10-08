use super::*;

fn session(id: &str, title: Option<&str>, history_len: usize, language: &str) -> SessionSummary {
    SessionSummary {
        id: id.to_owned(),
        workspace_root: "/work".to_owned(),
        origin_workspace_root: "/origin".to_owned(),
        title: title.map(str::to_owned),
        created_at_ms: 1_000,
        updated_at_ms: 1_700_000_000_123,
        conversation_language: language.to_owned(),
        history_len,
        has_checkpoint: false,
    }
}

#[test]
fn an_empty_listing_has_one_line_and_a_bare_json_object() {
    let snapshot = SessionListSnapshot {
        sessions: &[],
        has_more: false,
        next_cursor: None,
        skipped_invalid: 0,
        all_workspaces: false,
    };
    assert_eq!(
        snapshot.render(OutputFormat::Text),
        "[sessions] no saved sessions\n"
    );
    assert_eq!(
        snapshot.render(OutputFormat::Json),
        "{\"kind\":\"sessions\",\"count\":0,\"sessions\":[]}\n"
    );
}

#[test]
fn sessions_list_titles_details_paging_and_skipped_records() {
    let sessions = [
        session("abc", Some("Fix \u{1b}[31mbug"), 1, "en-US"),
        session("def", None, 2, "und-Cyrl"),
    ];
    let snapshot = SessionListSnapshot {
        sessions: &sessions,
        has_more: true,
        next_cursor: Some("v1:1700000000123:def".to_owned()),
        skipped_invalid: 1,
        all_workspaces: true,
    };
    assert_eq!(
        snapshot.render(OutputFormat::Text),
        "[sessions] 2 saved\n - Fix \\x1b[31mbug\n   id=abc | 1 turn | English | updated 2023-11-14 22:13:20.123 UTC\n - Untitled session\n   id=def | 2 turns | Cyrillic script | updated 2023-11-14 22:13:20.123 UTC\n[sessions] more saved sessions; continue with `oh-fx sessions --all --cursor v1:1700000000123:def`\n[sessions] warning: skipped 1 unreadable saved session; run `oh-fx doctor` for recovery guidance\n"
    );
    assert_eq!(
        snapshot.render(OutputFormat::Json),
        "{\"kind\":\"sessions\",\"count\":2,\"skipped_invalid\":1,\"has_more\":true,\"next_cursor\":\"v1:1700000000123:def\",\"sessions\":[{\"id\":\"abc\",\"title\":\"Fix \\u001b[31mbug\",\"preview\":null,\"workspace_root\":\"/work\",\"origin_workspace_root\":\"/origin\",\"created_at_ms\":1000,\"updated_at_ms\":1700000000123,\"history_len\":1,\"conversation_language\":\"en-US\"},{\"id\":\"def\",\"title\":\"Untitled session\",\"preview\":null,\"workspace_root\":\"/work\",\"origin_workspace_root\":\"/origin\",\"created_at_ms\":1000,\"updated_at_ms\":1700000000123,\"history_len\":2,\"conversation_language\":\"und-Cyrl\"}]}\n"
    );
}

#[test]
fn only_unreadable_sessions_say_so() {
    let snapshot = SessionListSnapshot {
        sessions: &[],
        has_more: false,
        next_cursor: None,
        skipped_invalid: 2,
        all_workspaces: false,
    };
    assert_eq!(
        snapshot.render(OutputFormat::Text),
        "[sessions] no readable saved sessions\n[sessions] warning: skipped 2 unreadable saved sessions; run `oh-fx doctor` for recovery guidance\n"
    );
    assert_eq!(
        snapshot.render(OutputFormat::Json),
        "{\"kind\":\"sessions\",\"count\":0,\"skipped_invalid\":2,\"sessions\":[]}\n"
    );
}

#[test]
fn languages_and_timestamps_follow_upstreams_labels() {
    assert_eq!(language_label("und"), None);
    assert_eq!(language_label("UND-latn"), Some("Latin script"));
    assert_eq!(language_label("und-Zzzz"), Some("und-Zzzz"));
    assert_eq!(language_label("pt-BR"), Some("Portuguese"));
    assert_eq!(language_label("tlh"), Some("tlh"));
    assert_eq!(language_label("abc界"), Some("abc界"));
    assert_eq!(language_label("und界"), Some("und界"));
    let sessions = [session("abc", None, 1, "abc界")];
    let snapshot = SessionListSnapshot {
        sessions: &sessions,
        has_more: false,
        next_cursor: None,
        skipped_invalid: 0,
        all_workspaces: false,
    };
    assert!(
        snapshot
            .render(OutputFormat::Text)
            .contains("id=abc | 1 turn | abc界 | updated")
    );
    assert_eq!(utc_timestamp(0), "1970-01-01 00:00:00.000 UTC");
    assert_eq!(utc_timestamp(-1), "unknown");
    assert_eq!(
        utc_timestamp(MAX_TIMESTAMP_MS),
        "9999-12-31 23:59:59.999 UTC"
    );
    assert_eq!(utc_timestamp(MAX_TIMESTAMP_MS + 1), "unknown");
}
