use super::*;

const ACTIVITY: SessionActivity = SessionActivity {
    api_duration_complete: true,
    wall_duration_complete: false,
    code_complete: true,
    api_duration_ms: 1200,
    wall_duration_ms: 3400,
    lines_added: 5,
    lines_removed: 2,
};

fn totals(input_tokens: u64, output_tokens: u64, total_cost: f64) -> SessionTotals {
    SessionTotals {
        total_cost,
        input_tokens,
        output_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
        request_count: None,
    }
}

fn source<'a>(
    completeness: UsageCompleteness,
    totals: SessionTotals,
    models: &'a [SessionModelSource<'a>],
) -> SessionUsageSource<'a> {
    SessionUsageSource {
        snapshot_time_ms: 200,
        session_started_at_ms: 100,
        completeness,
        totals,
        models,
        activity: ACTIVITY,
    }
}

#[test]
fn non_complete_session_reports_keep_known_totals_and_activity() {
    for completeness in [UsageCompleteness::Pending, UsageCompleteness::Incomplete] {
        let report = build_session_report(&source(completeness, totals(0, 0, 0.0), &[])).unwrap();
        assert_eq!(report.totals.unwrap().total_tokens, 0);
        assert_eq!(report.session_activity, Some(ACTIVITY));
        assert_eq!(report.completeness, completeness);
    }
    let legacy =
        build_session_report(&source(UsageCompleteness::Legacy, totals(0, 0, 0.0), &[])).unwrap();
    assert_eq!(legacy.totals, None);
    assert_eq!(legacy.session_activity, Some(ACTIVITY));
    assert_eq!(
        (legacy.window_start_ms, legacy.coverage_started_at_ms),
        (100, Some(100))
    );
    assert_eq!(legacy.coverage, UsageCoverage::Full);
}

#[test]
fn session_reports_copy_the_session_arithmetic_and_order_models_by_usage() {
    let models = [
        SessionModelSource {
            model: "provider/small",
            totals: totals(3, 1, 0.5),
        },
        SessionModelSource {
            model: "provider/large",
            totals: SessionTotals {
                request_count: Some(2),
                reasoning_tokens: Some(4),
                ..totals(30, 10, 0.25)
            },
        },
    ];
    let session = SessionTotals {
        request_count: Some(3),
        ..totals(40, 12, 1.0)
    };
    let report =
        build_session_report(&source(UsageCompleteness::Complete, session, &models)).unwrap();
    assert_eq!(report.scope, UsageScope::Session);
    assert_eq!(report.snapshot_time_ms, 200);
    let totals = report.totals.unwrap();
    assert_eq!((totals.total_tokens, totals.request_count), (52, Some(3)));
    assert_eq!(totals.total_cost.to_bits(), 1.0_f64.to_bits());
    let names: Vec<&str> = report
        .models
        .iter()
        .map(|model| model.model.as_str())
        .collect();
    assert_eq!(names, ["provider/large", "provider/small"]);
    assert_eq!(report.models[0].totals.total_tokens, 40);
    assert_eq!(report.models[0].totals.reasoning_tokens, Some(4));
    assert_eq!(report.models[1].totals.request_count, None);
}

#[test]
fn session_reports_reject_inconsistent_times_totals_and_models() {
    let complete = UsageCompleteness::Complete;
    let base = source(complete, totals(4, 2, 0.0), &[]);
    let rejected = [
        (
            SessionUsageSource {
                session_started_at_ms: 300,
                ..base
            },
            UsageReportError::InvalidSnapshotTime,
        ),
        (
            SessionUsageSource {
                snapshot_time_ms: -1,
                ..base
            },
            UsageReportError::InvalidSnapshotTime,
        ),
        (
            SessionUsageSource {
                totals: totals(4, 2, f64::NAN),
                ..base
            },
            UsageReportError::InvalidSnapshotTime,
        ),
        (
            SessionUsageSource {
                totals: SessionTotals {
                    cache_read_tokens: 5,
                    ..totals(4, 2, 0.0)
                },
                ..base
            },
            UsageReportError::InvalidSnapshotTime,
        ),
        (
            SessionUsageSource {
                totals: SessionTotals {
                    reasoning_tokens: Some(3),
                    ..totals(4, 2, 0.0)
                },
                ..base
            },
            UsageReportError::InvalidGenerationFact,
        ),
        (
            SessionUsageSource {
                totals: totals(u64::MAX, 1, 0.0),
                ..base
            },
            UsageReportError::UsageOverflow,
        ),
    ];
    for (source, error) in rejected {
        assert_eq!(build_session_report(&source), Err(error), "{source:?}");
    }
    for model in ["", "bad model", "provider/\u{e9}"] {
        let models = [SessionModelSource {
            model,
            totals: totals(1, 1, 0.0),
        }];
        assert_eq!(
            build_session_report(&source(complete, totals(1, 1, 0.0), &models)),
            Err(UsageReportError::InvalidGenerationFact)
        );
    }
    let crowded: Vec<String> = (0..=MAX_MODELS)
        .map(|index| format!("provider/m{index}"))
        .collect();
    let models: Vec<SessionModelSource<'_>> = crowded
        .iter()
        .map(|model| SessionModelSource {
            model,
            totals: totals(0, 0, 0.0),
        })
        .collect();
    assert_eq!(
        build_session_report(&source(complete, totals(0, 0, 0.0), &models)),
        Err(UsageReportError::UsageCapacityExceeded)
    );
}
