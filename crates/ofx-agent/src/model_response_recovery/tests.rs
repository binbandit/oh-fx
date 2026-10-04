use super::*;

const RATE_LIMITED: ModelRecoveryCause = ModelRecoveryCause::RateLimited;
const UNAVAILABLE: ModelRecoveryCause = ModelRecoveryCause::ProviderUnavailable;
const INTERRUPTED: ModelRecoveryCause = ModelRecoveryCause::NetworkInterrupted;
const CONNECTIVITY: ModelRecoveryCause = ModelRecoveryCause::ConnectivityLost;
const STREAM_TIMEOUT: ModelRecoveryCause = ModelRecoveryCause::ProviderStreamTimeout;

fn evidence(cause: ModelRecoveryCause) -> Evidence {
    Evidence {
        cause,
        retry_after_seconds: None,
        pacing: RetryPacing::Idle,
        progress: Progress::Unknown,
        output: Output::None,
        tool: ToolEvidence::None,
        recovery_elapsed: None,
    }
}

#[test]
fn model_response_recovery_policy_is_deterministic_and_never_pauses_transient_failure() {
    let base = evidence(INTERRUPTED);
    let first = decide(base);
    assert_eq!(first, decide(base));
    assert_eq!(first.strategy, Strategy::RetryRequest);
    assert_eq!(
        first.strategy.action(),
        Some(ModelRecoveryAction::RetryingRequest)
    );
    assert_eq!(first.delay, Duration::from_millis(250));
    assert_eq!(first.required_action, ModelRecoveryRequiredAction::None);
    let partial = Evidence {
        output: Output::Partial,
        ..base
    };
    assert_eq!(decide(partial).strategy, Strategy::ContinueResponse);
    let with_tool = |tool| decide(Evidence { tool, ..partial }).strategy;
    assert_eq!(
        with_tool(ToolEvidence::ProvenUnexecuted),
        Strategy::RegenerateTool
    );
    assert_eq!(with_tool(ToolEvidence::Uncertain), Strategy::ReconcileTool);
    assert_eq!(
        with_tool(ToolEvidence::Confirmed),
        Strategy::ContinueAfterTool
    );
    let actions = [
        Strategy::RegenerateTool,
        Strategy::ContinueAfterTool,
        Strategy::ReconcileTool,
    ]
    .map(Strategy::action);
    assert_eq!(
        actions,
        [
            Some(ModelRecoveryAction::RegeneratingTool),
            Some(ModelRecoveryAction::ContinuingAfterTool),
            Some(ModelRecoveryAction::ReconcilingTool),
        ]
    );
    for (cause, waiting) in [
        (CONNECTIVITY, Strategy::WaitForConnectivity),
        (STREAM_TIMEOUT, Strategy::ProbeLiveness),
    ] {
        let tool = ToolEvidence::ProvenUnexecuted;
        assert_eq!(
            decide(Evidence {
                tool,
                ..evidence(cause)
            })
            .strategy,
            waiting
        );
    }
}

#[test]
fn connectivity_loss_waits_without_consuming_the_attempt_budget() {
    let mut pacing = RetryPacing::Idle;
    for expected in [1, 2, 5, 5] {
        let waiting = decide(Evidence {
            pacing,
            ..evidence(CONNECTIVITY)
        });
        assert_eq!(waiting.strategy, Strategy::WaitForConnectivity);
        assert_eq!(
            waiting.strategy.action(),
            Some(ModelRecoveryAction::WaitingForConnectivity)
        );
        assert_eq!(waiting.delay, Duration::from_secs(expected));
        pacing = waiting.next_pacing;
    }
    let stalled = decide(Evidence {
        progress: Progress::Stalled,
        ..evidence(CONNECTIVITY)
    });
    assert_eq!(stalled.strategy, Strategy::WaitForConnectivity);
}

#[test]
fn stream_timeout_probes_liveness_instead_of_pausing() {
    let probing = decide(evidence(STREAM_TIMEOUT));
    assert_eq!(probing.strategy, Strategy::ProbeLiveness);
    assert_eq!(probing.required_action, ModelRecoveryRequiredAction::None);
    assert_eq!(
        probing.strategy.action(),
        Some(ModelRecoveryAction::CheckingLiveness)
    );
    assert_eq!(probing.delay, Duration::from_millis(250));
    let probing_again = decide(Evidence {
        pacing: probing.next_pacing,
        ..evidence(STREAM_TIMEOUT)
    });
    assert_eq!(probing_again.delay, Duration::from_secs(1));
}

#[test]
fn stalled_progress_stops_instead_of_restarting_forever() {
    let stalled_evidence = Evidence {
        progress: Progress::Stalled,
        ..evidence(INTERRUPTED)
    };
    let stalled = decide(stalled_evidence);
    assert_eq!(stalled.strategy, Strategy::Stop);
    assert_eq!(stalled.strategy.action(), None);
    assert_eq!(
        stalled.required_action,
        ModelRecoveryRequiredAction::SurfaceStall
    );
    let advancing = decide(Evidence {
        progress: Progress::Advancing,
        ..stalled_evidence
    });
    assert_eq!(advancing.strategy, Strategy::RetryRequest);
    let stopped = decide(Evidence {
        cause: STREAM_TIMEOUT,
        ..stalled_evidence
    });
    assert_eq!(stopped.strategy, Strategy::Stop);
    assert_eq!(
        stopped.required_action,
        ModelRecoveryRequiredAction::SurfaceStall
    );
}

#[test]
fn retry_after_is_honoured_up_to_the_cap() {
    for (hint, expected) in [(30, 30), (31, 30), (u64::MAX, 30), (4, 4)] {
        let decision = decide(Evidence {
            retry_after_seconds: Some(hint),
            ..evidence(RATE_LIMITED)
        });
        assert_eq!(decision.strategy, Strategy::RetryRequest);
        assert_eq!(decision.delay, Duration::from_secs(expected));
        assert_eq!(decision.next_pacing, RetryPacing::Idle);
    }
}

#[test]
fn retry_schedule_uses_the_approved_cap() {
    let expected = [
        Duration::from_millis(250),
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(4),
        Duration::from_secs(8),
        Duration::from_secs(16),
        Duration::from_secs(30),
        Duration::from_secs(30),
        Duration::from_secs(30),
    ];
    let mut total = Duration::ZERO;
    for (attempt, delay) in (1..).zip(expected) {
        assert_eq!(retry_delay(attempt), delay);
        total += delay;
    }
    assert_eq!(total, Duration::from_millis(121_250));
}

#[test]
fn billable_retries_throttle_past_the_recovery_window_without_dying() {
    let within_window = decide(evidence(UNAVAILABLE));
    assert_eq!(within_window.delay, Duration::from_millis(250));
    let past_window = Evidence {
        recovery_elapsed: Some(BILLABLE_RETRY_WINDOW + Duration::from_nanos(1)),
        ..evidence(UNAVAILABLE)
    };
    let throttled = decide(past_window);
    assert_eq!(throttled.strategy, Strategy::RetryRequest);
    assert_eq!(throttled.delay, THROTTLED_RETRY_DELAY);
    let hinted = decide(Evidence {
        retry_after_seconds: Some(1),
        ..past_window
    });
    assert_eq!(hinted.delay, THROTTLED_RETRY_DELAY);
    let at_window = decide(Evidence {
        recovery_elapsed: Some(BILLABLE_RETRY_WINDOW),
        ..evidence(UNAVAILABLE)
    });
    assert_eq!(at_window.delay, Duration::from_millis(250));
}

#[test]
fn a_timeout_past_the_recovery_window_waits_the_billable_minimum() {
    let past_window = Evidence {
        progress: Progress::Advancing,
        recovery_elapsed: Some(BILLABLE_RETRY_WINDOW + Duration::from_nanos(1)),
        ..evidence(STREAM_TIMEOUT)
    };
    let throttled = decide(past_window);
    assert_eq!(throttled.strategy, Strategy::ProbeLiveness);
    assert_eq!(throttled.delay, THROTTLED_RETRY_DELAY);
    let at_window = decide(Evidence {
        recovery_elapsed: Some(BILLABLE_RETRY_WINDOW),
        ..past_window
    });
    assert_eq!(at_window.delay, Duration::from_millis(250));
}

#[test]
fn partial_output_restarts_the_response_unless_the_connection_is_down_or_silent() {
    let partial = |cause| {
        decide(Evidence {
            output: Output::Partial,
            ..evidence(cause)
        })
    };
    for cause in [INTERRUPTED, UNAVAILABLE, RATE_LIMITED] {
        let restart = partial(cause);
        assert_eq!(restart.strategy, Strategy::ContinueResponse, "{cause:?}");
        assert_eq!(restart.delay, decide(evidence(cause)).delay, "{cause:?}");
    }
    assert_eq!(
        partial(CONNECTIVITY).strategy,
        Strategy::WaitForConnectivity
    );
    assert_eq!(partial(STREAM_TIMEOUT).strategy, Strategy::ProbeLiveness);
    assert_eq!(
        Strategy::ContinueResponse.action(),
        Some(ModelRecoveryAction::ContinuingResponse)
    );
}

#[test]
fn implicit_retry_pacing_is_independent_from_the_shared_attempt_budget() {
    let first = decide(evidence(INTERRUPTED));
    let second = decide(Evidence {
        pacing: first.next_pacing,
        ..evidence(INTERRUPTED)
    });
    assert_eq!(second.delay, Duration::from_secs(1));
    let switched = decide(Evidence {
        pacing: second.next_pacing,
        ..evidence(UNAVAILABLE)
    });
    assert_eq!(switched.delay, Duration::from_millis(250));
    let zero_hinted = decide(Evidence {
        pacing: switched.next_pacing,
        retry_after_seconds: Some(0),
        ..evidence(UNAVAILABLE)
    });
    assert_eq!(zero_hinted.delay, Duration::from_secs(1));
    assert!(matches!(
        zero_hinted.next_pacing,
        RetryPacing::Implicit { .. }
    ));
    let zero_again = decide(Evidence {
        pacing: zero_hinted.next_pacing,
        retry_after_seconds: Some(0),
        ..evidence(UNAVAILABLE)
    });
    assert_eq!(zero_again.delay, Duration::from_secs(2));
    let timed = decide(Evidence {
        pacing: zero_again.next_pacing,
        retry_after_seconds: Some(4),
        ..evidence(UNAVAILABLE)
    });
    assert_eq!(timed.delay, Duration::from_secs(4));
    assert_eq!(timed.next_pacing, RetryPacing::Idle);
}

#[test]
fn repeated_failures_at_the_same_point_count_as_a_stall() {
    let mut tracker = ProgressTracker::default();
    assert_eq!(tracker.observe(0), Progress::Unknown);
    assert_eq!(tracker.observe(0), Progress::Advancing);
    assert_eq!(tracker.observe(0), Progress::Stalled);
    assert_eq!(tracker.observe(12), Progress::Advancing);
    assert_eq!(tracker.observe(12), Progress::Advancing);
    assert_eq!(tracker.observe(12), Progress::Stalled);
}

#[test]
fn network_and_stream_failures_carry_progress_evidence_and_http_status_failures_do_not() {
    let stream_failure = ProviderError::new(ProviderErrorKind::ServerError, "ProviderError");
    let mut http_failure = stream_failure.clone();
    http_failure.status = Some(503);
    for cause in [INTERRUPTED, CONNECTIVITY, STREAM_TIMEOUT, UNAVAILABLE] {
        assert!(tracks_progress(cause, &stream_failure), "{cause:?}");
    }
    for cause in [UNAVAILABLE, RATE_LIMITED] {
        assert!(!tracks_progress(cause, &http_failure), "{cause:?}");
    }
    assert!(!tracks_progress(RATE_LIMITED, &stream_failure));
    let mut recovery = Recovery::default();
    for _ in 0..3 {
        let decision = recovery.decide(
            UNAVAILABLE,
            &http_failure,
            12,
            (Output::None, ToolEvidence::None),
        );
        assert_eq!(decision.strategy, Strategy::RetryRequest);
    }
    let mut recovery = Recovery::default();
    let decisions: Vec<Strategy> = (0..3)
        .map(|_| {
            recovery
                .decide(
                    UNAVAILABLE,
                    &stream_failure,
                    12,
                    (Output::None, ToolEvidence::None),
                )
                .strategy
        })
        .collect();
    assert_eq!(
        decisions,
        [
            Strategy::RetryRequest,
            Strategy::RetryRequest,
            Strategy::Stop
        ]
    );
    assert_eq!(
        recovery_cause(ProviderErrorKind::Timeout),
        Some(STREAM_TIMEOUT)
    );
    assert_eq!(
        recovery_cause(ProviderErrorKind::TransportInterrupted),
        Some(INTERRUPTED)
    );
}
