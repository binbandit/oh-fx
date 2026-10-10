use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;

use ofx_contract::{HookRuntime, HookScope, TurnPresentationOutcome};

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Started(TurnId),
    Finished(TurnId, TurnOutcome),
    TurnEnd(HookScope, TurnId, TurnPresentationOutcome, bool),
}

type Log = Arc<Mutex<Vec<Seen>>>;

fn observed(agent: Agent, scope: HookScope) -> (Agent, Log) {
    gated(agent, scope, None)
}

fn gated(agent: Agent, scope: HookScope, gate: Option<mpsc::Receiver<()>>) -> (Agent, Log) {
    let log = Log::default();
    let recorded = Arc::clone(&log);
    let caller = thread::current().id();
    let gate = Mutex::new(gate);
    let mut hooks = HookRuntime::default();
    hooks
        .register_post_turn_end("test.turn_end", move |input| {
            if let Some(gate) = &*gate.lock().unwrap() {
                let _ = gate.recv_timeout(Duration::from_secs(5));
            }
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
            &mut |event| match event {
                UiEvent::TurnStarted { turn_id } => {
                    log.lock().unwrap().push(Seen::Started(turn_id));
                }
                UiEvent::TurnFinished { turn_id, outcome } => {
                    log.lock().unwrap().push(Seen::Finished(turn_id, outcome));
                }
                _ => {}
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
    agent.settle_lifecycle().await;
    let turn_end =
        |turn, outcome| Seen::TurnEnd(HookScope::Interactive, TurnId::new(turn), outcome, false);
    assert_eq!(
        *log.lock().unwrap(),
        [
            Seen::Started(TurnId::new(1)),
            Seen::Finished(TurnId::new(1), TurnOutcome::Completed),
            turn_end(1, TurnPresentationOutcome::Completed),
            Seen::Started(TurnId::new(2)),
            Seen::Finished(TurnId::new(2), TurnOutcome::Failed),
            turn_end(2, TurnPresentationOutcome::Failed),
            Seen::Started(TurnId::new(3)),
            Seen::Finished(TurnId::new(3), TurnOutcome::Interrupted),
            turn_end(3, TurnPresentationOutcome::Interrupted),
        ]
    );
}

#[tokio::test]
async fn a_turn_returns_before_its_post_turn_end_handlers_finish_and_the_next_waits_for_them() {
    let provider = FakeProvider::new(vec![text_reply("done"), text_reply("again")]);
    let (release, gate) = mpsc::channel();
    let (mut agent, log) = gated(
        new_agent(provider, Vec::new()),
        HookScope::Interactive,
        Some(gate),
    );
    let first = tokio::time::timeout(
        Duration::from_secs(2),
        run_logged(&mut agent, &log, "one", &CancellationToken::new()),
    )
    .await;
    assert!(first.is_ok());
    assert_eq!(
        *log.lock().unwrap(),
        [
            Seen::Started(TurnId::new(1)),
            Seen::Finished(TurnId::new(1), TurnOutcome::Completed),
        ]
    );
    let releasing = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        drop(release);
    });
    run_logged(&mut agent, &log, "two", &CancellationToken::new()).await;
    releasing.join().unwrap();
    agent.settle_lifecycle().await;
    let turn_end = |turn| {
        Seen::TurnEnd(
            HookScope::Interactive,
            TurnId::new(turn),
            TurnPresentationOutcome::Completed,
            false,
        )
    };
    assert_eq!(
        *log.lock().unwrap(),
        [
            Seen::Started(TurnId::new(1)),
            Seen::Finished(TurnId::new(1), TurnOutcome::Completed),
            turn_end(1),
            Seen::Started(TurnId::new(2)),
            Seen::Finished(TurnId::new(2), TurnOutcome::Completed),
            turn_end(2),
        ]
    );
}

#[tokio::test]
async fn post_turn_end_carries_the_scope_the_agent_was_given() {
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let (mut agent, log) = observed(new_agent(provider, Vec::new()), HookScope::Ask);
    run(&mut agent, "one").await;
    agent.settle_lifecycle().await;
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
    agent.settle_lifecycle().await;
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
