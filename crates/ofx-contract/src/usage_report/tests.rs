use super::*;

const NOW: i64 = MS_PER_DAY * 40;

fn fact(
    id: &str,
    model: &str,
    created_at_ms: i64,
    tokens: (u64, u64),
    cost: f64,
) -> GenerationFact {
    GenerationFact {
        id: id.to_owned(),
        created_at_ms,
        model: model.to_owned(),
        input_tokens: tokens.0,
        output_tokens: tokens.1,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: Some(0),
        billable_web_search_calls: 0,
        total_cost: cost,
    }
}

fn incident(occurred_at_ms: i64, completeness: UsageCompleteness) -> UsageIncident {
    UsageIncident {
        occurred_at_ms,
        completeness,
    }
}

#[test]
fn rolling_reports_use_closed_open_boundaries_and_stable_usage_first_ordering() {
    let facts = [
        fact(
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "z/model",
            NOW - MS_PER_DAY * 30,
            (3, 2),
            1.0,
        ),
        fact(
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAW",
            "b/model",
            NOW - 1,
            (5, 5),
            2.0,
        ),
        fact(
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAX",
            "a/model",
            NOW - 2,
            (5, 5),
            2.0,
        ),
        fact(
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAY",
            "excluded/model",
            NOW,
            (100, 100),
            100.0,
        ),
    ];
    let report = build_rolling_report(UsageScope::Days30, NOW, Some(0), &facts, &[]).unwrap();

    assert_eq!(report.coverage, UsageCoverage::Full);
    assert_eq!(report.window_start_ms, NOW - MS_PER_DAY * 30);
    assert_eq!(report.totals.unwrap().total_tokens, 25);
    let models: Vec<&str> = report
        .models
        .iter()
        .map(|model| model.model.as_str())
        .collect();
    assert_eq!(models, ["a/model", "b/model", "z/model"]);
}

#[test]
fn rolling_reports_dedupe_exact_facts_and_fail_closed_on_conflicts_and_incidents() {
    let original = fact(
        "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "provider/model",
        NOW - 1,
        (4, 2),
        0.5,
    );
    let mut conflict = original.clone();
    conflict.output_tokens = 3;

    let exact = build_rolling_report(
        UsageScope::Hours24,
        NOW,
        Some(0),
        &[original.clone(), original.clone()],
        &[],
    )
    .unwrap();
    assert_eq!(exact.completeness, UsageCompleteness::Complete);
    assert_eq!(exact.totals.unwrap().total_tokens, 6);
    assert_eq!(exact.totals.unwrap().request_count, Some(1));

    let conflicted = build_rolling_report(
        UsageScope::Hours24,
        NOW,
        Some(0),
        &[original.clone(), conflict],
        &[],
    )
    .unwrap();
    assert_eq!(conflicted.completeness, UsageCompleteness::Incomplete);
    assert_eq!(conflicted.totals.unwrap().total_tokens, 6);

    let pending = build_rolling_report(
        UsageScope::Hours24,
        NOW,
        Some(0),
        std::slice::from_ref(&original),
        &[incident(NOW - 1, UsageCompleteness::Pending)],
    )
    .unwrap();
    assert_eq!(pending.completeness, UsageCompleteness::Pending);
    assert_eq!(pending.totals.unwrap().total_tokens, 6);
}

#[test]
fn rolling_reports_distinguish_tracking_start_from_measured_zero() {
    let unstarted = build_rolling_report(UsageScope::Days7, NOW, None, &[], &[]).unwrap();
    assert_eq!(unstarted.coverage, UsageCoverage::NotStarted);
    assert_eq!(unstarted.completeness, UsageCompleteness::Complete);
    assert_eq!(unstarted.totals, None);

    let concurrently_started =
        build_rolling_report(UsageScope::Days7, NOW, Some(NOW + 1), &[], &[]).unwrap();
    assert_eq!(concurrently_started.coverage, UsageCoverage::NotStarted);
    assert_eq!(concurrently_started.coverage_started_at_ms, None);
    assert_eq!(concurrently_started.totals, None);

    let unknown = build_rolling_report(
        UsageScope::Days7,
        NOW,
        None,
        &[],
        &[incident(NOW - 1, UsageCompleteness::Incomplete)],
    )
    .unwrap();
    assert_eq!(unknown.coverage, UsageCoverage::NotStarted);
    assert_eq!(unknown.completeness, UsageCompleteness::Incomplete);
    assert_eq!(unknown.totals, None);

    let measured =
        build_rolling_report(UsageScope::Days7, NOW, Some(NOW - MS_PER_DAY), &[], &[]).unwrap();
    assert_eq!(measured.coverage, UsageCoverage::Partial);
    assert_eq!(measured.coverage_started_at_ms, Some(NOW - MS_PER_DAY));
    let totals = measured.totals.unwrap();
    assert_eq!(totals.total_tokens, 0);
    assert_eq!(totals.reasoning_tokens, None);
    assert_eq!(totals.request_count, Some(0));
    assert!(measured.models.is_empty());
}

#[test]
fn incidents_count_only_inside_the_window_and_legacy_ones_read_as_incomplete() {
    let outside = [
        incident(NOW - MS_PER_DAY - 1, UsageCompleteness::Incomplete),
        incident(NOW, UsageCompleteness::Incomplete),
    ];
    let report = build_rolling_report(UsageScope::Hours24, NOW, Some(0), &[], &outside).unwrap();
    assert_eq!(report.completeness, UsageCompleteness::Complete);

    let legacy = [incident(NOW - MS_PER_DAY, UsageCompleteness::Legacy)];
    let report = build_rolling_report(UsageScope::Hours24, NOW, Some(0), &[], &legacy).unwrap();
    assert_eq!(report.completeness, UsageCompleteness::Incomplete);

    let mixed = [
        incident(NOW - 2, UsageCompleteness::Incomplete),
        incident(NOW - 1, UsageCompleteness::Pending),
    ];
    let report = build_rolling_report(UsageScope::Hours24, NOW, Some(0), &[], &mixed).unwrap();
    assert_eq!(report.completeness, UsageCompleteness::Incomplete);
}

#[test]
fn totals_keep_reasoning_only_while_every_fact_reports_it() {
    let mut first = fact(
        "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "provider/model",
        NOW - 2,
        (10, 4),
        0.25,
    );
    first.reasoning_tokens = Some(3);
    first.cache_read_tokens = 6;
    first.cache_write_tokens = 2;
    let mut second = fact(
        "gen_01ARZ3NDEKTSV4RRFFQ69G5FAW",
        "provider/model",
        NOW - 1,
        (1, 1),
        0.5,
    );
    second.reasoning_tokens = None;
    let third = fact(
        "gen_01ARZ3NDEKTSV4RRFFQ69G5FAX",
        "other/model",
        NOW - 1,
        (2, 2),
        0.0,
    );

    let report = build_rolling_report(
        UsageScope::Hours24,
        NOW,
        Some(0),
        &[first, second, third],
        &[],
    )
    .unwrap();
    let totals = report.totals.unwrap();
    assert_eq!(
        totals,
        UsageTotals {
            total_tokens: 20,
            input_tokens: 13,
            output_tokens: 7,
            cache_read_tokens: 6,
            cache_write_tokens: 2,
            reasoning_tokens: None,
            request_count: Some(3),
            total_cost: 0.75,
        }
    );
    assert_eq!(report.models[0].model, "provider/model");
    assert_eq!(report.models[0].totals.reasoning_tokens, None);
    assert_eq!(report.models[1].totals.reasoning_tokens, Some(0));
}

#[test]
fn rolling_reports_reject_invalid_facts_times_and_scopes() {
    let valid = fact(
        "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "provider/model",
        NOW - MS_PER_DAY * 2,
        (4, 2),
        0.5,
    );
    let mut invalid = Vec::new();
    for change in [
        (|fact: &mut GenerationFact| fact.id = "resp_1".to_owned()) as fn(&mut GenerationFact),
        |fact| fact.created_at_ms = -1,
        |fact| fact.model = String::new(),
        |fact| fact.model = "two words".to_owned(),
        |fact| fact.model = "m".repeat(1025),
        |fact| fact.total_cost = -0.5,
        |fact| fact.total_cost = f64::NAN,
        |fact| fact.cache_read_tokens = 5,
        |fact| fact.cache_write_tokens = 5,
        |fact| fact.reasoning_tokens = Some(3),
    ] {
        let mut changed = valid.clone();
        change(&mut changed);
        invalid.push(changed);
    }
    for fact in invalid {
        assert_eq!(
            build_rolling_report(
                UsageScope::Hours24,
                NOW,
                Some(0),
                std::slice::from_ref(&fact),
                &[]
            ),
            Err(UsageReportError::InvalidGenerationFact),
            "{fact:?}"
        );
    }
    assert!(valid.is_valid());
    assert!(
        build_rolling_report(UsageScope::Hours24, NOW, Some(0), &[valid], &[])
            .unwrap()
            .models
            .is_empty()
    );
    assert_eq!(
        build_rolling_report(UsageScope::Session, NOW, Some(0), &[], &[]),
        Err(UsageReportError::InvalidSnapshotTime)
    );
    assert_eq!(
        build_rolling_report(UsageScope::Hours24, -1, Some(0), &[], &[]),
        Err(UsageReportError::InvalidSnapshotTime)
    );
    assert_eq!(
        build_rolling_report(UsageScope::Hours24, NOW, Some(-1), &[], &[]),
        Err(UsageReportError::InvalidSnapshotTime)
    );
    assert_eq!(
        build_rolling_report(UsageScope::Days30, i64::MIN + 1, Some(0), &[], &[]),
        Err(UsageReportError::InvalidSnapshotTime)
    );
}

#[test]
fn rolling_reports_fail_on_overflow_and_too_many_models() {
    let huge = |id: &str, created_at_ms| GenerationFact {
        input_tokens: u64::MAX,
        ..fact(id, "provider/model", created_at_ms, (0, 0), 0.0)
    };
    let facts = [
        huge("gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", NOW - 2),
        huge("gen_01ARZ3NDEKTSV4RRFFQ69G5FAW", NOW - 1),
    ];
    assert_eq!(
        build_rolling_report(UsageScope::Hours24, NOW, Some(0), &facts, &[]),
        Err(UsageReportError::UsageOverflow)
    );
    let mut totals_overflow = huge("gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", NOW - 1);
    totals_overflow.output_tokens = 1;
    assert_eq!(
        build_rolling_report(UsageScope::Hours24, NOW, Some(0), &[totals_overflow], &[]),
        Err(UsageReportError::UsageOverflow)
    );
    let costly = |id: &str| fact(id, "provider/model", NOW - 1, (0, 0), f64::MAX);
    assert_eq!(
        build_rolling_report(
            UsageScope::Hours24,
            NOW,
            Some(0),
            &[
                costly("gen_01ARZ3NDEKTSV4RRFFQ69G5FAV"),
                costly("gen_01ARZ3NDEKTSV4RRFFQ69G5FAW"),
            ],
            &[],
        ),
        Err(UsageReportError::UsageOverflow)
    );

    let many: Vec<GenerationFact> = (0..=MAX_MODELS)
        .map(|index| {
            fact(
                &format!("gen_{index:026}"),
                &format!("provider/model-{index}"),
                NOW - 1,
                (1, 1),
                0.0,
            )
        })
        .collect();
    assert_eq!(
        build_rolling_report(UsageScope::Hours24, NOW, Some(0), &many[..MAX_MODELS], &[])
            .unwrap()
            .models
            .len(),
        MAX_MODELS
    );
    assert_eq!(
        build_rolling_report(UsageScope::Hours24, NOW, Some(0), &many, &[]),
        Err(UsageReportError::UsageCapacityExceeded)
    );
}

#[test]
fn utc_dates_use_short_month_names_and_reject_negative_times() {
    assert_eq!(format_utc_date(0), "Jan 1, 1970");
    assert_eq!(format_utc_date(1_775_045_467_123), "Apr 1, 2026");
    assert_eq!(format_utc_date(951_782_400_000), "Feb 29, 2000");
    assert_eq!(format_utc_date(1_798_761_599_999), "Dec 31, 2026");
    assert_eq!(format_utc_date(-1), "Unknown");
}

#[test]
fn scopes_carry_upstream_labels_and_cli_values() {
    let scopes = [
        (UsageScope::Session, "Session", None),
        (UsageScope::Hours24, "24 hours", Some("24h")),
        (UsageScope::Days7, "7 days", Some("7d")),
        (UsageScope::Days30, "30 days", Some("30d")),
    ];
    for (scope, label, value) in scopes {
        assert_eq!(scope.label(), label);
        assert_eq!(scope.cli_value(), value);
        if let Some(value) = value {
            assert_eq!(UsageScope::parse_cli_value(value), Some(scope));
        }
    }
    assert_eq!(UsageScope::parse_cli_value("session"), None);
    assert_eq!(UsageScope::parse_cli_value("1d"), None);
    for completeness in [
        UsageCompleteness::Complete,
        UsageCompleteness::Pending,
        UsageCompleteness::Incomplete,
        UsageCompleteness::Legacy,
    ] {
        assert_eq!(
            UsageCompleteness::parse(completeness.name()),
            Some(completeness)
        );
    }
    assert_eq!(UsageCompleteness::parse("Complete"), None);
}
