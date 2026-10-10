use std::sync::Mutex;
use std::thread;

use ofx_contract::{HookRuntime, HookScope, TurnPresentationOutcome};

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Finished(TurnId, TurnOutcome),
    TurnEnd(HookScope, TurnId, TurnPresentationOutcome, bool),
}

type Log = Arc<Mutex<Vec<Seen>>>;

fn observed(agent: Agent, scope: HookScope) -> (Agent, Log) {
    let log = Log::default();
    let recorded = Arc::clone(&log);
    let caller = thread::current().id();
    let mut hooks = HookRuntime::default();
    hooks
        .register_post_turn_end("test.turn_end", move |input| {
            recorded.lock().unwrap().push(Seen::TurnEnd(
                input.invocation.scope,
                input.invocation.turn_id,
                input.outcome,
                thread::current().id() == caller,
            ));
        })
        .unwrap();
    (agent.with_lifecycle(hooks.freeze(), scope), log)
}

async fn run_logged(agent: &mut Agent, log: &Log, prompt: &str, cancel: &CancellationToken) {
    agent
        .run_turn(
            prompt,
            &mut |event| {
                if let UiEvent::TurnFinished { turn_id, outcome } = event {
                    log.lock().unwrap().push(Seen::Finished(turn_id, outcome));
                }
            },
            cancel,
        )
        .await;
}

#[tokio::test]
async fn post_turn_end_follows_each_finished_turn_off_the_runtime_thread() {
    let provider = FakeProvider::new(vec![
        text_reply("done"),
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::InvalidRequest, "BadRequest"),
        ),
    ]);
    let (mut agent, log) = observed(new_agent(provider, Vec::new()), HookScope::Interactive);
    run_logged(&mut agent, &log, "one", &CancellationToken::new()).await;
    run_logged(&mut agent, &log, "two", &CancellationToken::new()).await;
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    run_logged(&mut agent, &log, "three", &cancelled).await;
    let turn_end =
        |turn, outcome| Seen::TurnEnd(HookScope::Interactive, TurnId::new(turn), outcome, false);
    assert_eq!(
        *log.lock().unwrap(),
        [
            Seen::Finished(TurnId::new(1), TurnOutcome::Completed),
            turn_end(1, TurnPresentationOutcome::Completed),
            Seen::Finished(TurnId::new(2), TurnOutcome::Failed),
            turn_end(2, TurnPresentationOutcome::Failed),
            Seen::Finished(TurnId::new(3), TurnOutcome::Interrupted),
            turn_end(3, TurnPresentationOutcome::Interrupted),
        ]
    );
}

#[tokio::test]
async fn post_turn_end_carries_the_scope_the_agent_was_given() {
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let (mut agent, log) = observed(new_agent(provider, Vec::new()), HookScope::Ask);
    run(&mut agent, "one").await;
    assert_eq!(
        *log.lock().unwrap(),
        [Seen::TurnEnd(
            HookScope::Ask,
            TurnId::new(1),
            TurnPresentationOutcome::Completed,
            false
        )]
    );
}

#[tokio::test(start_paused = true)]
async fn a_paused_turn_reports_paused_to_post_turn_end() {
    let provider = FakeProvider::new(vec![Script::Fail(
        Vec::new(),
        failure(ProviderErrorKind::ConnectivityLost, "ConnectionFailed"),
    )]);
    let (mut agent, log) = observed(new_agent(provider, Vec::new()), HookScope::Interactive);
    let pause = agent.recovery_pause();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let report = agent
        .run_turn(
            "go",
            &mut |event| {
                if matches!(&event, UiEvent::Recovery { status, .. } if status.retry_wait.is_some())
                {
                    pause.request();
                    trigger.cancel();
                }
            },
            &cancel,
        )
        .await;
    assert_eq!(report.failure.unwrap().code(), "RecoveryPaused");
    assert_eq!(
        *log.lock().unwrap(),
        [Seen::TurnEnd(
            HookScope::Interactive,
            TurnId::new(1),
            TurnPresentationOutcome::Paused,
            false
        )]
    );
}
