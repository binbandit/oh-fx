use ofx_contract::{DeliveryOutcome, INTERRUPTED_BEFORE_COMPLETION, INTERRUPTED_TURN_CONTEXT};

use super::turn_log::{Accounted, MemoryLog, logged, logged_turn};
use super::*;

fn accounting_agent(
    provider: Arc<FakeProvider>,
    log: MemoryLog,
) -> (Agent, Arc<Mutex<Vec<Accounted>>>) {
    let accounted = Arc::clone(&log.accounted);
    (
        logged(new_agent(provider, vec![echo_tool()]), Box::new(log)),
        accounted,
    )
}

#[tokio::test]
async fn each_admitted_request_is_begun_and_settled_and_committed_lines_are_recorded() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"lines":true}"#)]),
        text_reply("done"),
    ]);
    let (mut agent, accounted) = accounting_agent(provider, MemoryLog::default());
    let (report, _) = run(&mut agent, "edit").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        *accounted.lock().unwrap(),
        [
            Accounted::Begun(1),
            Accounted::Finished(1, DeliveryOutcome::PossiblyBilledWithoutIdentity),
            Accounted::Lines(3, 1),
            Accounted::Begun(2),
            Accounted::Finished(2, DeliveryOutcome::PossiblyBilledWithoutIdentity),
        ]
    );
}

#[tokio::test]
async fn failed_requests_settle_by_whether_they_could_have_been_billed() {
    for (error, outcome) in [
        (
            http_failure(ProviderErrorKind::Unauthorized, "Unauthorized", 401),
            DeliveryOutcome::Unbilled,
        ),
        (
            failure(ProviderErrorKind::ConnectionFailed, "ConnectionFailed"),
            DeliveryOutcome::Unbilled,
        ),
        (
            failure(ProviderErrorKind::ConnectivityLost, "ConnectionFailed"),
            DeliveryOutcome::Unbilled,
        ),
        (
            failure(ProviderErrorKind::Protocol, "InvalidChunk"),
            DeliveryOutcome::AmbiguousDelivery,
        ),
    ] {
        let provider = FakeProvider::new(vec![Script::Fail(Vec::new(), error.clone())]);
        let (mut agent, accounted) = accounting_agent(provider, MemoryLog::default());
        run(&mut agent, "hi").await;
        let accounted = accounted.lock().unwrap().clone();
        assert_eq!(accounted.first(), Some(&Accounted::Begun(1)), "{error:?}");
        assert_eq!(
            accounted.get(1),
            Some(&Accounted::Finished(1, outcome)),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn a_request_refused_before_admission_is_not_accounted() {
    let provider = FakeProvider::new(vec![Script::Refuse(failure(
        ProviderErrorKind::InvalidRequest,
        "InvalidRequest",
    ))]);
    let (mut agent, accounted) = accounting_agent(provider, MemoryLog::default());
    run(&mut agent, "hi").await;
    assert!(accounted.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_cancelled_request_may_have_been_billed() {
    let provider = FakeProvider::new(vec![Script::WaitForCancel]);
    let (mut agent, accounted) = accounting_agent(provider, MemoryLog::default());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move { trigger.cancel() });
    let report = agent.run_turn("go", &mut |_| {}, &cancel).await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(
        *accounted.lock().unwrap(),
        [
            Accounted::Begun(1),
            Accounted::Finished(1, DeliveryOutcome::AmbiguousDelivery),
        ]
    );
}

#[tokio::test]
async fn a_request_that_cannot_be_accounted_fails_the_turn_with_the_session_error() {
    for log in [
        MemoryLog {
            refused_request: Some("SessionPathUnsafe"),
            ..MemoryLog::default()
        },
        MemoryLog {
            refused_settlement: Some("SessionPersistenceUncertain"),
            ..MemoryLog::default()
        },
    ] {
        let expected = log
            .refused_request
            .or(log.refused_settlement)
            .unwrap()
            .to_owned();
        let provider = FakeProvider::new(vec![text_reply("never kept")]);
        let (mut agent, _) = accounting_agent(provider, log);
        let (report, _) = run(&mut agent, "hi").await;
        assert_eq!(report.outcome, TurnOutcome::Failed);
        assert_eq!(
            report.failure.map(|failure| failure.code().to_owned()),
            Some(expected)
        );
    }
}

#[tokio::test]
async fn an_answer_streamed_before_its_settlement_failed_stays_in_history_and_the_log() {
    let provider = FakeProvider::new(vec![text_reply("streamed answer"), text_reply("next")]);
    let log = MemoryLog {
        refused_settlement: Some("SessionPersistenceUncertain"),
        ..MemoryLog::default()
    };
    let entries = Arc::clone(&log.entries);
    let (mut agent, _) = accounting_agent(Arc::clone(&provider), log);
    let (report, _) = run(&mut agent, "hi").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(
        report.failure.map(|failure| failure.code().to_owned()),
        Some("SessionPersistenceUncertain".to_owned())
    );
    assert_eq!(
        *entries.lock().unwrap(),
        [logged_turn("hi", &[], r#"Failed "streamed answer""#)]
    );

    run(&mut agent, "next").await;
    assert_eq!(
        provider.requests()[1].messages,
        [
            ChatMessage::user("hi"),
            ChatMessage::Assistant {
                content: Some(format!(
                    "streamed answer\n\n{INTERRUPTED_BEFORE_COMPLETION}"
                )),
                tool_calls: Vec::new(),
                provider_replay: None,
            },
            ChatMessage::user(INTERRUPTED_TURN_CONTEXT),
            ChatMessage::user("next"),
        ]
    );
}
