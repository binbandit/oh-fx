use super::*;

#[test]
fn recovery_names_are_upstreams_strategy_tags() {
    let names: Vec<&str> = [
        Recovering::Retry(Strategy::RetryRequest),
        Recovering::Retry(Strategy::ContinueResponse),
        Recovering::Retry(Strategy::RegenerateTool),
        Recovering::Retry(Strategy::ContinueAfterTool),
        Recovering::Retry(Strategy::ReconcileTool),
        Recovering::Retry(Strategy::WaitForConnectivity),
        Recovering::Retry(Strategy::ProbeLiveness),
        Recovering::Stop,
        Recovering::Stall,
        Recovering::Pause,
        Recovering::Cancelled,
    ]
    .into_iter()
    .map(Recovering::name)
    .collect();
    assert_eq!(
        names,
        [
            "retry_request",
            "continue_response",
            "regenerate_tool",
            "continue_after_confirmed_tool",
            "reconcile_tool",
            "wait_for_connectivity",
            "probe_liveness",
            "stop",
            "stop",
            "pause",
            "stop",
        ]
    );
    assert!(Recovering::Retry(Strategy::ProbeLiveness).retries());
    assert!(!Recovering::Stall.retries());
    assert!(!Recovering::Cancelled.retries());
}

#[test]
fn checkpoint_causes_and_actions_use_upstreams_tags() {
    assert_eq!(
        cause_name(ModelRecoveryCause::NetworkInterrupted),
        "transport_interrupted"
    );
    assert_eq!(
        cause_name(ModelRecoveryCause::ProviderUnavailable),
        "provider_unavailable"
    );
    assert_eq!(
        cause_name(ModelRecoveryCause::ProviderStreamTimeout),
        "provider_stream_timeout"
    );
    assert_eq!(
        progress_name(RecoveryProgress::Waiting(
            ModelRecoveryAction::ContinuingAfterTool
        )),
        "continue_after_confirmed_tool"
    );
    assert_eq!(
        progress_name(RecoveryProgress::Waiting(
            ModelRecoveryAction::CheckingLiveness
        )),
        "probe_liveness"
    );
    assert_eq!(progress_name(RecoveryProgress::Paused), "pause");
}
