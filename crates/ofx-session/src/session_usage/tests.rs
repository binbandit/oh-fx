use crate::json_fields::parse_json;

use super::*;

const IDENTITY: &str = "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4";
const FX_SNAPSHOT: &str = concat!(
    "{\"schema_version\":3,\"billing\":\"pending\",\"api_duration_complete\":true,",
    "\"wall_duration_complete\":true,\"code_complete\":true,\"next_sequence\":4,",
    "\"settled_through_sequence\":3,\"api_duration_ms\":1500,\"wall_duration_ms\":60000,",
    "\"total_cost\":0.0125,\"input_tokens\":140,\"output_tokens\":40,\"cache_read_tokens\":20,",
    "\"cache_write_tokens\":0,\"reasoning_tokens\":null,\"request_count\":2,",
    "\"billable_web_search_calls\":1,\"lines_added\":7,\"lines_removed\":2,\"models\":[",
    "{\"model\":\"openai/gpt-5\",\"first_sequence\":1,\"total_cost\":0.0125,",
    "\"input_tokens\":120,\"output_tokens\":30,\"cache_read_tokens\":20,",
    "\"cache_write_tokens\":0,\"reasoning_tokens\":5,\"request_count\":1,",
    "\"billable_web_search_calls\":1},",
    "{\"model\":\"codex/gpt-5-codex\",\"first_sequence\":2,\"total_cost\":0,",
    "\"input_tokens\":20,\"output_tokens\":10,\"cache_read_tokens\":0,",
    "\"cache_write_tokens\":0,\"reasoning_tokens\":null,\"request_count\":1,",
    "\"billable_web_search_calls\":0}],\"pending\":[",
    "{\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAW\",\"sequence\":3,\"provider\":\"gateway\",",
    "\"origin\":\"https://ai-gateway.vercel.sh\",\"team\":\"team_1\",",
    "\"credential_source\":\"ai_gateway_api_key\",\"credential_identity\":",
    "\"8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4\",",
    "\"account_id\":null,\"observed_at_ms\":1775045467000}],",
    "\"publication_backlog\":[{\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAX\",",
    "\"created_at_ms\":1775045466000,\"model\":\"codex/gpt-5-codex\",\"input_tokens\":20,",
    "\"output_tokens\":10,\"cache_read_tokens\":0,\"cache_write_tokens\":0,",
    "\"reasoning_tokens\":null,\"billable_web_search_calls\":0,\"total_cost\":0}],",
    "\"incidents\":[{\"occurred_at_ms\":1775045466500,\"completeness\":\"incomplete\"}]}"
);

fn parsed(text: &str) -> Result<UsageSnapshot, UsageSnapshotError> {
    let value = parse_json(text.as_bytes()).map_err(|_| UsageSnapshotError::Invalid)?;
    UsageSnapshot::parse_rich(&value)
}

fn written(snapshot: &UsageSnapshot) -> String {
    let mut out = String::new();
    snapshot.write_rich(&mut out).unwrap();
    out
}

#[test]
fn a_snapshot_fx_wrote_reads_and_writes_back_byte_for_byte() {
    let snapshot = parsed(FX_SNAPSHOT).unwrap();
    assert_eq!(snapshot.billing, Availability::Pending);
    assert_eq!(snapshot.models.len(), 2);
    assert_eq!(snapshot.models[0].reasoning_tokens, Some(5));
    let pending = &snapshot.pending[0];
    assert_eq!(pending.provider, ProviderId::Gateway);
    assert_eq!(pending.team.as_deref(), Some("team_1"));
    assert_eq!(pending.credential_source, Some("ai_gateway_api_key"));
    assert_eq!(
        pending
            .credential_identity
            .map(|identity| lowercase_hex(&identity)),
        Some(IDENTITY.to_owned())
    );
    assert_eq!(snapshot.publication_backlog.len(), 1);
    assert_eq!(
        snapshot.incidents,
        [UsageIncident {
            occurred_at_ms: 1_775_045_466_500,
            completeness: UsageCompleteness::Incomplete,
        }]
    );
    assert_eq!(written(&snapshot), FX_SNAPSHOT);
}

#[test]
fn a_fresh_snapshot_writes_upstreams_zero_state() {
    assert_eq!(
        written(&UsageSnapshot::fresh()),
        "{\"schema_version\":3,\"billing\":\"complete\",\"api_duration_complete\":true,\"wall_duration_complete\":true,\"code_complete\":true,\"next_sequence\":1,\"settled_through_sequence\":0,\"api_duration_ms\":0,\"wall_duration_ms\":0,\"total_cost\":0,\"input_tokens\":0,\"output_tokens\":0,\"cache_read_tokens\":0,\"cache_write_tokens\":0,\"reasoning_tokens\":0,\"request_count\":0,\"billable_web_search_calls\":0,\"lines_added\":0,\"lines_removed\":0,\"models\":[],\"pending\":[],\"publication_backlog\":[],\"incidents\":[]}"
    );
    assert_eq!(
        written(&UsageSnapshot::unavailable()),
        "{\"schema_version\":3,\"billing\":\"incomplete\",\"api_duration_complete\":false,\"wall_duration_complete\":false,\"code_complete\":false,\"next_sequence\":1,\"settled_through_sequence\":0,\"api_duration_ms\":0,\"wall_duration_ms\":0,\"total_cost\":0,\"input_tokens\":0,\"output_tokens\":0,\"cache_read_tokens\":0,\"cache_write_tokens\":0,\"reasoning_tokens\":null,\"request_count\":null,\"billable_web_search_calls\":0,\"lines_added\":0,\"lines_removed\":0,\"models\":[],\"pending\":[],\"publication_backlog\":[],\"incidents\":[]}"
    );
}

#[test]
fn schema_two_and_older_pending_entries_are_read_and_rewritten_as_schema_three() {
    let schema_two = FX_SNAPSHOT.replacen("\"schema_version\":3", "\"schema_version\":2", 1);
    assert_eq!(written(&parsed(&schema_two).unwrap()), FX_SNAPSHOT);

    let plain_pending = "{\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAW\",\"sequence\":3,\"origin\":\"https://ai-gateway.vercel.sh\",\"team\":null}";
    let start = FX_SNAPSHOT
        .find("{\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAW\"")
        .unwrap();
    let end = FX_SNAPSHOT[start..].find('}').unwrap() + start + 1;
    for (pending, observed) in [
        (plain_pending.to_owned(), "null"),
        (
            plain_pending.replace("\"team\":null", "\"team\":null,\"observed_at_ms\":9"),
            "9",
        ),
    ] {
        let snapshot = parsed(&format!(
            "{}{pending}{}",
            &FX_SNAPSHOT[..start],
            &FX_SNAPSHOT[end..]
        ))
        .unwrap();
        assert_eq!(snapshot.pending[0].provider, ProviderId::Gateway);
        assert_eq!(snapshot.pending[0].credential_source, None);
        assert!(written(&snapshot).contains(&format!(
            "\"provider\":\"gateway\",\"origin\":\"https://ai-gateway.vercel.sh\",\"team\":null,\"credential_source\":null,\"credential_identity\":null,\"account_id\":null,\"observed_at_ms\":{observed}}}"
        )));
    }
}

const RULE_BREAKS: &[(&str, &str)] = &[
    ("\"next_sequence\":4", "\"next_sequence\":0"),
    (
        "\"settled_through_sequence\":3",
        "\"settled_through_sequence\":4",
    ),
    (
        "\"total_cost\":0.0125,\"input_tokens\":140",
        "\"total_cost\":-1,\"input_tokens\":140",
    ),
    (
        "\"total_cost\":0.0125,\"input_tokens\":140",
        "\"total_cost\":0.5,\"input_tokens\":140",
    ),
    ("\"billing\":\"pending\"", "\"billing\":\"complete\""),
    ("\"billing\":\"pending\"", "\"billing\":\"Pending\""),
    ("\"input_tokens\":140", "\"input_tokens\":141"),
    (
        "\"reasoning_tokens\":null,\"request_count\":2",
        "\"reasoning_tokens\":0,\"request_count\":2",
    ),
    ("\"request_count\":2", "\"request_count\":3"),
    (
        "\"billable_web_search_calls\":1,\"lines_added\"",
        "\"billable_web_search_calls\":0,\"lines_added\"",
    ),
    (
        "\"model\":\"codex/gpt-5-codex\",\"first_sequence\":2",
        "\"model\":\"openai/gpt-5\",\"first_sequence\":2",
    ),
    (
        "\"model\":\"codex/gpt-5-codex\",\"first_sequence\":2",
        "\"model\":\"codex/gpt-5-codex\",\"first_sequence\":1",
    ),
    (
        "\"model\":\"codex/gpt-5-codex\",\"first_sequence\":2",
        "\"model\":\"codex/gpt-5-codex\",\"first_sequence\":4",
    ),
    (
        "\"model\":\"openai/gpt-5\",\"first_sequence\":1",
        "\"model\":\"open ai\",\"first_sequence\":1",
    ),
    (
        "\"cache_read_tokens\":20,\"cache_write_tokens\":0,\"reasoning_tokens\":5",
        "\"cache_read_tokens\":121,\"cache_write_tokens\":0,\"reasoning_tokens\":5",
    ),
    ("\"reasoning_tokens\":5", "\"reasoning_tokens\":31"),
    (
        "\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAW\"",
        "\"id\":\"resp_1\"",
    ),
    ("\"sequence\":3", "\"sequence\":0"),
    ("\"sequence\":3", "\"sequence\":4"),
    (
        "\"origin\":\"https://ai-gateway.vercel.sh\"",
        "\"origin\":\"has space\"",
    ),
    ("\"team\":\"team_1\"", "\"team\":\"\""),
    ("\"provider\":\"gateway\"", "\"provider\":\"codex\""),
    ("\"provider\":\"gateway\"", "\"provider\":\"bad name\""),
    (
        "\"credential_source\":\"ai_gateway_api_key\"",
        "\"credential_source\":null",
    ),
    (
        "\"credential_source\":\"ai_gateway_api_key\"",
        "\"credential_source\":\"keychain\"",
    ),
    (
        "\"credential_identity\":\"8f43",
        "\"credential_identity\":\"8F43",
    ),
    ("\"account_id\":null", "\"account_id\":\"\""),
    ("\"observed_at_ms\":1775045467000", "\"observed_at_ms\":-1"),
    (
        "\"publication_backlog\":[{\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAX\"",
        "\"publication_backlog\":[{\"id\":\"resp_2\"",
    ),
    (
        "\"completeness\":\"incomplete\"",
        "\"completeness\":\"complete\"",
    ),
    ("\"schema_version\":3", "\"schema_version\":4"),
    ("\"schema_version\":3", "\"schema_version\":1"),
    ("\"lines_removed\":2,", "\"lines_removed\":2,\"extra\":1,"),
    ("\"lines_removed\":2,", ""),
    (
        ",\"billable_web_search_calls\":0}],\"pending\"",
        "}],\"pending\"",
    ),
];

#[test]
fn snapshots_that_break_upstreams_rules_are_rejected() {
    for (from, to) in RULE_BREAKS {
        assert!(FX_SNAPSHOT.contains(from), "{from}");
        assert!(
            parsed(&FX_SNAPSHOT.replacen(from, to, 1)).is_err(),
            "{from} -> {to}"
        );
    }
    let backlog_start = FX_SNAPSHOT
        .find("[{\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAX\"")
        .unwrap()
        + 1;
    let backlog_end = FX_SNAPSHOT[backlog_start..].find('}').unwrap() + backlog_start + 1;
    let fact = &FX_SNAPSHOT[backlog_start..backlog_end];
    let duplicated = FX_SNAPSHOT.replacen(fact, &format!("{fact},{fact}"), 1);
    assert_eq!(parsed(&duplicated), Err(UsageSnapshotError::Invalid));
}

#[test]
fn capacity_limits_follow_upstream() {
    let mut snapshot = UsageSnapshot::fresh();
    for index in 0..=MAX_USAGE_INCIDENTS {
        snapshot
            .append_incident(UsageIncident {
                occurred_at_ms: i64::try_from(index).unwrap() + 10,
                completeness: UsageCompleteness::Pending,
            })
            .unwrap();
    }
    assert_eq!(
        snapshot.incidents,
        [UsageIncident {
            occurred_at_ms: 26,
            completeness: UsageCompleteness::Incomplete,
        }]
    );
    snapshot
        .append_incident(UsageIncident {
            occurred_at_ms: 26,
            completeness: UsageCompleteness::Incomplete,
        })
        .unwrap();
    assert_eq!(snapshot.incidents.len(), 1);
    assert_eq!(
        snapshot.append_incident(UsageIncident {
            occurred_at_ms: 1,
            completeness: UsageCompleteness::Legacy,
        }),
        Err(UsageSnapshotError::Invalid)
    );

    let model = |index: u64| ModelAggregate {
        model: format!("provider/model-{index}"),
        first_sequence: index + 1,
        total_cost: 0.0,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: Some(0),
        request_count: Some(1),
        billable_web_search_calls: 0,
    };
    let mut many = UsageSnapshot::fresh();
    many.next_sequence = 100;
    many.settled_through_sequence = 99;
    many.models = (0..32).map(model).collect();
    many.request_count = Some(32);
    assert_eq!(many.validate(), Ok(()));
    many.models.push(model(32));
    many.request_count = Some(33);
    assert_eq!(many.validate(), Err(UsageSnapshotError::CapacityExceeded));

    let mut long = UsageSnapshot::fresh();
    long.next_sequence = 10;
    long.settled_through_sequence = 9;
    long.models = (0..9)
        .map(|index| ModelAggregate {
            model: format!("{index}{}", "m".repeat(1000)),
            ..model(index)
        })
        .collect();
    long.request_count = Some(9);
    assert_eq!(long.validate(), Err(UsageSnapshotError::CapacityExceeded));
    long.models.pop();
    long.request_count = Some(8);
    assert_eq!(long.validate(), Ok(()));
}

#[test]
fn credential_sources_authorize_only_their_own_providers() {
    let configured = ProviderId::Configured("portkey".to_owned());
    for (source, provider, authorized) in [
        ("ai_gateway_api_key", &ProviderId::Gateway, true),
        ("fx_login", &ProviderId::Gateway, true),
        ("chatgpt_subscription", &ProviderId::Gateway, false),
        ("chatgpt_subscription", &ProviderId::Codex, true),
        ("ai_gateway_api_key", &ProviderId::Codex, false),
        ("grok_subscription", &ProviderId::Grok, true),
        ("configured", &configured, true),
        ("configured", &ProviderId::Gateway, false),
        ("stored_key", &configured, false),
        ("host_managed", &ProviderId::Codex, true),
    ] {
        assert_eq!(
            authorizes(source, provider),
            authorized,
            "{source} {provider:?}"
        );
    }
}

const LEGACY: &str = "{\"billing\":\"complete\",\"api_duration_complete\":true,\"wall_duration_complete\":true,\"code_complete\":true,\"next_sequence\":2,\"settled_through_sequence\":1,\"api_duration_ms\":10,\"wall_duration_ms\":20,\"total_cost\":1,\"input_tokens\":10,\"output_tokens\":3,\"cache_read_tokens\":2,\"cache_write_tokens\":0,\"billable_web_search_calls\":0,\"lines_added\":0,\"lines_removed\":0,\"models\":[{\"model\":\"test/model\",\"first_sequence\":1,\"total_cost\":1,\"input_tokens\":10,\"output_tokens\":3,\"cache_read_tokens\":2,\"cache_write_tokens\":0,\"billable_web_search_calls\":0}],\"pending\":[]}";

fn legacy(text: &str) -> Result<UsageSnapshot, UsageSnapshotError> {
    let value = parse_json(text.as_bytes()).map_err(|_| UsageSnapshotError::Invalid)?;
    UsageSnapshot::parse_legacy(&value)
}

#[test]
fn legacy_usage_keeps_valid_snapshots_and_strict_parsing() {
    let strict = parsed(LEGACY).unwrap();
    assert_eq!(legacy(LEGACY).unwrap(), strict);
    assert_eq!(strict.reasoning_tokens, None);
    assert_eq!(strict.request_count, None);
    assert_eq!(strict.models[0].request_count, None);

    for field in ["cache_read_tokens", "cache_write_tokens"] {
        let separate = LEGACY.replace(
            &format!(
                "\"{field}\":{}",
                if field == "cache_read_tokens" { 2 } else { 0 }
            ),
            &format!("\"{field}\":11"),
        );
        let unavailable = legacy(&separate).unwrap();
        assert_eq!(unavailable, UsageSnapshot::empty(Availability::Legacy));
        assert_eq!(unavailable.validate(), Ok(()));
        assert_eq!(parsed(&separate), Err(UsageSnapshotError::Invalid));
    }

    let rewritten = written(&strict);
    assert_eq!(legacy(&rewritten).unwrap(), strict);
    let separate_rich = rewritten.replace("\"cache_read_tokens\":2", "\"cache_read_tokens\":11");
    assert_eq!(legacy(&separate_rich), Err(UsageSnapshotError::Invalid));
}

#[test]
fn legacy_usage_does_not_hide_malformed_accounting() {
    let separate = LEGACY.replace("\"cache_read_tokens\":2", "\"cache_read_tokens\":11");
    for (from, to) in [
        (
            "\"total_cost\":1,\"input_tokens\":10,\"output_tokens\":3,\"cache_read_tokens\":11,\"cache_write_tokens\":0,\"billable",
            "\"total_cost\":-1,\"input_tokens\":10,\"output_tokens\":3,\"cache_read_tokens\":11,\"cache_write_tokens\":0,\"billable",
        ),
        (
            "\"input_tokens\":10,\"output_tokens\":3,\"cache_read_tokens\":11,\"cache_write_tokens\":0,\"billable",
            "\"input_tokens\":9,\"output_tokens\":3,\"cache_read_tokens\":11,\"cache_write_tokens\":0,\"billable",
        ),
        ("\"next_sequence\":2", "\"next_sequence\":0"),
        ("\"first_sequence\":1", "\"first_sequence\":0"),
        (
            "\"pending\":[]",
            "\"pending\":[{\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"sequence\":0,\"origin\":\"https://ai-gateway.vercel.sh\",\"team\":null}]",
        ),
        ("\"pending\":[]", "\"pending\":[],\"unknown\":null"),
    ] {
        assert!(separate.contains(from), "{from}");
        assert!(legacy(&separate.replacen(from, to, 1)).is_err(), "{to}");
    }
    let model = "{\"model\":\"test/model\",\"first_sequence\":1,\"total_cost\":1,\"input_tokens\":10,\"output_tokens\":3,\"cache_read_tokens\":11,\"cache_write_tokens\":0,\"billable_web_search_calls\":0}";
    let doubled = separate
        .replace(&format!("[{model}]"), &format!("[{model},{model}]"))
        .replace("\"total_cost\":1,\"input_tokens\":10,\"output_tokens\":3,\"cache_read_tokens\":11,\"cache_write_tokens\":0,\"billable", "\"total_cost\":2,\"input_tokens\":20,\"output_tokens\":6,\"cache_read_tokens\":22,\"cache_write_tokens\":0,\"billable");
    assert_eq!(legacy(&doubled), Err(UsageSnapshotError::Invalid));
    assert_eq!(legacy("null"), Err(UsageSnapshotError::Invalid));
}
