use std::sync::Mutex;

use ofx_contract::{HookHandlerError, HookRuntime, HookScope, RecoveryProgress, StopAction};

use super::turn_log::{Logged, MemoryLog, logged};
use super::*;

const CONTINUED: &str = "Continue the turn. oh-fx hook context:\nverify the answer";

type Seen = Arc<Mutex<Vec<(String, usize, bool)>>>;

fn stopped(
    agent: Agent,
    action: impl Fn() -> Result<StopAction, HookHandlerError> + Send + Sync + 'static,
) -> (Agent, Seen, Arc<Mutex<Vec<Logged>>>) {
    let seen = Seen::default();
    let recorded = Arc::clone(&seen);
    let mut hooks = HookRuntime::default();
    hooks
        .register_stop("test.stop", move |input| {
            recorded.lock().unwrap().push((
                input.assistant_text.to_owned(),
                input.step_index,
                input.can_continue,
            ));
            action()
        })
        .unwrap();
    let (log, entries) = MemoryLog::shared();
    let agent = logged(
        agent.with_lifecycle(hooks.freeze(), HookScope::Interactive),
        log,
    );
    (agent, seen, entries)
}

fn verify() -> Result<StopAction, HookHandlerError> {
    Ok(StopAction::ContinueOnce("verify the answer".to_owned()))
}

fn turn(steps: &[&str], end: &str) -> Logged {
    Logged::Turn {
        user: "go".to_owned(),
        steps: steps.iter().map(|step| (*step).to_owned()).collect(),
        steering: Vec::new(),
        files: Vec::new(),
        end: end.to_owned(),
    }
}

fn standalone(text: &str, replay: bool) -> String {
    format!("{text:?} replay={replay} calls=[] results=[]")
}

#[tokio::test]
async fn an_allowed_answer_is_saved_as_a_standalone_step_with_an_empty_reply() {
    let provider = FakeProvider::new(vec![text_reply("candidate")]);
    let (mut agent, seen, entries) =
        stopped(new_agent(provider, Vec::new()), || Ok(StopAction::Allow));
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "candidate");
    assert_eq!(*seen.lock().unwrap(), [("candidate".to_owned(), 1, true)]);
    assert_eq!(
        *entries.lock().unwrap(),
        [turn(
            &[&standalone("candidate", false)],
            r#"replied "" replay=false"#
        )]
    );
    assert_eq!(
        agent.history,
        [
            ChatMessage::user("go"),
            ChatMessage::Assistant {
                content: Some("candidate".to_owned()),
                tool_calls: Vec::new(),
                provider_replay: None,
            },
        ]
    );
}

#[tokio::test]
async fn a_continuation_runs_once_and_the_answers_join_while_the_history_keeps_both() {
    let provider = FakeProvider::new(vec![
        text_reply("candidate"),
        text_reply("final"),
        text_reply("follow-up"),
    ]);
    let (mut agent, seen, entries) = stopped(new_agent(Arc::clone(&provider), Vec::new()), verify);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "candidate\nfinal");
    assert_eq!(seen.lock().unwrap().len(), 1);
    let requests = provider.requests();
    assert_eq!(
        requests[1].messages[1..],
        [
            ChatMessage::Assistant {
                content: Some("candidate".to_owned()),
                tool_calls: Vec::new(),
                provider_replay: None,
            },
            ChatMessage::user(CONTINUED),
        ]
    );
    assert_eq!(
        *entries.lock().unwrap(),
        [turn(
            &[&standalone("candidate", false)],
            r#"replied "final" replay=false"#
        )]
    );
    run(&mut agent, "next").await;
    let follow_up = &provider.requests()[2].messages;
    assert!(
        follow_up
            .iter()
            .all(|message| *message != ChatMessage::user(CONTINUED))
    );
    assert_eq!(
        follow_up[..3],
        [
            ChatMessage::user("go"),
            ChatMessage::Assistant {
                content: Some("candidate".to_owned()),
                tool_calls: Vec::new(),
                provider_replay: None,
            },
            ChatMessage::Assistant {
                content: Some("final".to_owned()),
                tool_calls: Vec::new(),
                provider_replay: None,
            },
        ]
    );
}

#[tokio::test]
async fn both_answers_keep_their_own_replay() {
    let provider = FakeProvider::new(vec![
        with_replay(text_reply("candidate"), "first"),
        with_replay(text_reply("final"), "last"),
    ]);
    let (mut agent, _, entries) = stopped(new_agent(provider, Vec::new()), verify);
    run(&mut agent, "go").await;
    assert_eq!(
        *entries.lock().unwrap(),
        [turn(
            &[&standalone("candidate", true)],
            r#"replied "final" replay=true"#
        )]
    );
}

#[tokio::test]
async fn a_continuation_without_budget_finishes_with_the_candidate() {
    let provider = FakeProvider::new(vec![text_reply("candidate")]);
    let config = AgentConfig {
        step_limit: 1,
        ..config()
    };
    let agent = Agent::new(
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        Vec::new(),
        Arc::new(FixedContext),
        Arc::new(ArgumentGate),
        config,
    );
    let (mut agent, seen, entries) = stopped(agent, verify);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(*seen.lock().unwrap(), [("candidate".to_owned(), 1, false)]);
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(
        *entries.lock().unwrap(),
        [turn(
            &[&standalone("candidate", false)],
            r#"replied "" replay=false"#
        )]
    );
}

#[tokio::test]
async fn handler_errors_let_the_candidate_finish_the_turn() {
    for error in [HookHandlerError::Failed, HookHandlerError::Cancelled] {
        let provider = FakeProvider::new(vec![text_reply("candidate")]);
        let (mut agent, seen, entries) =
            stopped(new_agent(provider, Vec::new()), move || Err(error));
        let (report, _) = run(&mut agent, "go").await;
        assert_eq!(report.outcome, TurnOutcome::Completed);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(
            *entries.lock().unwrap(),
            [turn(
                &[&standalone("candidate", false)],
                r#"replied "" replay=false"#
            )]
        );
    }
}

#[tokio::test]
async fn cancelling_during_the_hook_interrupts_and_keeps_the_candidate_as_a_step() {
    let provider = FakeProvider::new(vec![with_replay(text_reply("candidate"), "original")]);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let (mut agent, seen, entries) = stopped(new_agent(provider, Vec::new()), move || {
        trigger.cancel();
        Ok(StopAction::ContinueOnce("ignored".to_owned()))
    });
    let report = agent.run_turn("go", &mut |_| {}, &cancel).await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(
        *entries.lock().unwrap(),
        [turn(&[&standalone("candidate", true)], r#"Cancelled """#)]
    );
}

#[tokio::test]
async fn a_later_stream_failure_keeps_the_candidate_and_saves_the_partial_reply() {
    let provider = FakeProvider::new(vec![
        text_reply("candidate"),
        Script::Fail(
            vec![StreamEvent::TextDelta {
                text: "partial".to_owned(),
            }],
            failure(ProviderErrorKind::InvalidRequest, "BadRequest"),
        ),
    ]);
    let (mut agent, _, entries) = stopped(new_agent(provider, Vec::new()), verify);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(
        *entries.lock().unwrap(),
        [turn(
            &[&standalone("candidate", false)],
            r#"replied "partial" replay=false"#
        )]
    );
}

#[tokio::test]
async fn a_later_silent_failure_keeps_the_candidate_with_an_empty_reply() {
    let provider = FakeProvider::new(vec![
        text_reply("candidate"),
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::InvalidRequest, "BadRequest"),
        ),
    ]);
    let (mut agent, _, entries) = stopped(new_agent(provider, Vec::new()), verify);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(
        *entries.lock().unwrap(),
        [turn(
            &[&standalone("candidate", false)],
            r#"replied "" replay=false"#
        )]
    );
}

#[tokio::test]
async fn an_interruption_after_the_continuation_keeps_the_candidate_and_the_partial() {
    let provider = FakeProvider::new(vec![
        text_reply("candidate"),
        Script::StreamThenWait(vec![StreamEvent::TextDelta {
            text: "PARTIAL".to_owned(),
        }]),
    ]);
    let (mut agent, _, entries) = stopped(new_agent(provider, Vec::new()), verify);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let report = agent
        .run_turn(
            "go",
            &mut |event| {
                if matches!(&event, UiEvent::AssistantText { text, .. } if text == "PARTIAL") {
                    trigger.cancel();
                }
            },
            &cancel,
        )
        .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(
        *entries.lock().unwrap(),
        [turn(
            &[&standalone("candidate", false)],
            r#"Cancelled "PARTIAL""#
        )]
    );
}

#[tokio::test]
async fn the_step_limit_after_a_continuation_keeps_the_candidate_and_the_tool_step() {
    let provider = FakeProvider::new(vec![
        text_reply("candidate"),
        tool_reply(&[("call-1", r#"{"text":"a"}"#)]),
    ]);
    let config = AgentConfig {
        step_limit: 2,
        ..config()
    };
    let agent = Agent::new(
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        vec![echo_tool()],
        Arc::new(FixedContext),
        Arc::new(ArgumentGate),
        config,
    );
    let (mut agent, _, entries) = stopped(agent, verify);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.failure, Some(TurnFailure::StepLimitReached));
    let entries = entries.lock().unwrap().clone();
    let Logged::Turn { steps, end, .. } = &entries[0] else {
        panic!("{entries:?}");
    };
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0], standalone("candidate", false));
    assert!(steps[1].contains(r#"calls=["call-1"]"#), "{}", steps[1]);
    assert_eq!(
        *end,
        format!("replied {:?} replay=false", STEP_LIMIT_NOTICE)
    );
}

#[tokio::test]
async fn the_hook_waits_for_the_answer_after_the_silent_tool_summary() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        tool_reply(&[("call-2", "{}")]),
        text_reply(""),
        text_reply("summary"),
    ]);
    let (mut agent, seen, _) = stopped(new_agent(Arc::clone(&provider), vec![echo_tool()]), || {
        Ok(StopAction::Allow)
    });
    run(&mut agent, "go").await;
    assert_eq!(*seen.lock().unwrap(), [("summary".to_owned(), 4, true)]);
    assert_eq!(provider.requests().len(), 4);
}

#[tokio::test(start_paused = true)]
async fn a_pause_after_the_continuation_saves_the_candidate_in_its_checkpoint() {
    let provider = FakeProvider::new(vec![
        text_reply("candidate"),
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::ConnectivityLost, "ConnectionFailed"),
        ),
    ]);
    let (mut agent, _, entries) = stopped(new_agent(provider, Vec::new()), verify);
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
    let entries = entries.lock().unwrap().clone();
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, Logged::Turn { .. }))
    );
    let paused = entries
        .iter()
        .rev()
        .find_map(|entry| match entry {
            Logged::Recovery {
                steps,
                progress: RecoveryProgress::Paused,
                ..
            } => Some(steps.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(paused, [standalone("candidate", false)]);
}

#[tokio::test]
async fn cancelling_before_the_hook_runs_keeps_the_candidate_without_calling_it() {
    let provider = FakeProvider::new(vec![text_reply("")]);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let (mut agent, seen, entries) =
        stopped(new_agent(provider, Vec::new()), || Ok(StopAction::Allow));
    let report = agent
        .run_turn(
            "go",
            &mut |event| {
                if matches!(event, UiEvent::Operational { .. }) {
                    trigger.cancel();
                }
            },
            &cancel,
        )
        .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(
        *entries.lock().unwrap(),
        [turn(
            &[&standalone(EMPTY_RESPONSE_TEXT, false)],
            r#"Cancelled """#
        )]
    );
}
