use std::path::PathBuf;

use super::turn_log::{Logged, MemoryLog, logged};
use super::*;
use crate::execution_memory::{history_turn, steering_message};
use crate::worker_runtime::{QueuedPrompt, WorkerRuntime};

fn steered_agent(provider: &Arc<FakeProvider>) -> (Agent, Arc<WorkerRuntime>) {
    let worker = Arc::new(WorkerRuntime::default());
    let agent =
        new_agent(Arc::clone(provider), vec![echo_tool()]).with_steering(Arc::clone(&worker));
    (agent, worker)
}

fn plain(id: u64, text: &str) -> QueuedPrompt {
    QueuedPrompt::new(id, text.to_owned(), Vec::new())
}

fn with_skill(id: u64, text: &str) -> QueuedPrompt {
    QueuedPrompt::new(
        id,
        text.to_owned(),
        vec![SkillBinding {
            name: "review".to_owned(),
            path: PathBuf::from("/skills/review/SKILL.md"),
        }],
    )
}

fn streaming(text: &str) -> Script {
    Script::StreamThenWait(vec![StreamEvent::TextDelta {
        text: text.to_owned(),
    }])
}

fn assistant(text: &str) -> ChatMessage {
    ChatMessage::Assistant {
        content: Some(text.to_owned()),
        tool_calls: Vec::new(),
        provider_replay: None,
    }
}

fn calling(id: &str, arguments: &str) -> ChatMessage {
    ChatMessage::Assistant {
        content: None,
        tool_calls: vec![echo_call(id, arguments)],
        provider_replay: None,
    }
}

fn applied(events: &[UiEvent]) -> Vec<(u64, &str)> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::SteeringApplied { prompt, text, .. } => Some((*prompt, text.as_str())),
            _ => None,
        })
        .collect()
}

async fn run_steered(
    agent: &mut Agent,
    worker: &Arc<WorkerRuntime>,
    cancel: &CancellationToken,
    mut on_event: impl FnMut(&UiEvent, &WorkerRuntime, &CancellationToken) + Send,
) -> (TurnReport, Vec<UiEvent>) {
    let prompt = worker.take_next().expect("a queued prompt");
    let mut events = Vec::new();
    let report = agent
        .run_turn(
            &prompt.text,
            &mut |event| {
                on_event(&event, worker, cancel);
                events.push(event);
            },
            cancel,
        )
        .await;
    worker.finish_processing();
    (report, events)
}

#[tokio::test]
async fn a_steer_typed_while_the_reply_streams_continues_the_same_turn() {
    let provider = FakeProvider::new(vec![
        streaming("Looking at the parser"),
        text_reply("Checked the tests too."),
    ]);
    let (mut agent, worker) = steered_agent(&provider);
    worker.admit(plain(0, "fix the parser"));
    let mut sent = false;
    let (report, events) = run_steered(
        &mut agent,
        &worker,
        &CancellationToken::new(),
        |event, worker, _| {
            if matches!(event, UiEvent::AssistantText { .. }) && !sent {
                sent = true;
                worker.admit(plain(1, "check the tests too"));
            }
        },
    )
    .await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "Checked the tests too.");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].messages,
        [
            ChatMessage::user("fix the parser"),
            assistant("Looking at the parser"),
            ChatMessage::user(steering_message("check the tests too")),
        ]
    );
    assert_eq!(applied(&events), [(1, "check the tests too")]);
}

#[tokio::test]
async fn steering_typed_while_a_tool_runs_rides_the_request_after_its_result() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"text":"one"}"#)]),
        text_reply("Done."),
    ]);
    let (mut agent, worker) = steered_agent(&provider);
    worker.admit(plain(0, "go"));
    let (report, events) = run_steered(
        &mut agent,
        &worker,
        &CancellationToken::new(),
        |event, worker, _| {
            if matches!(event, UiEvent::ToolStarted { .. }) {
                worker.admit(plain(1, "also run the linter"));
                worker.admit(plain(2, "and the formatter"));
            }
        },
    )
    .await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(finished(&events), [("call-1", ToolResultStatus::Success)]);
    assert_eq!(
        provider.requests()[1].messages,
        [
            ChatMessage::user("go"),
            calling("call-1", r#"{"text":"one"}"#),
            tool_message(
                "call-1",
                r#"echo {"text":"one"}"#,
                ToolResultStatus::Success
            ),
            ChatMessage::user(steering_message("also run the linter")),
            ChatMessage::user(steering_message("and the formatter")),
        ]
    );
    assert_eq!(
        applied(&events),
        [(1, "also run the linter"), (2, "and the formatter")]
    );
}

#[tokio::test]
async fn steering_waiting_at_the_final_answer_continues_the_turn() {
    let provider = FakeProvider::new(vec![text_reply("First answer."), text_reply("Second.")]);
    let (mut agent, worker) = steered_agent(&provider);
    worker.admit(plain(0, "go"));
    let mut sent = false;
    let (report, events) = run_steered(
        &mut agent,
        &worker,
        &CancellationToken::new(),
        |event, worker, _| {
            if matches!(event, UiEvent::AssistantText { .. }) && !sent {
                sent = true;
                worker.enter_tool_phase();
                worker.admit(plain(1, "one more thing"));
            }
        },
    )
    .await;
    assert_eq!(report.final_text, "Second.");
    assert_eq!(
        provider.requests()[1].messages,
        [
            ChatMessage::user("go"),
            assistant("First answer."),
            ChatMessage::user(steering_message("one more thing")),
        ]
    );
    assert_eq!(applied(&events), [(1, "one more thing")]);
}

#[tokio::test]
async fn steering_with_skills_ends_the_turn_at_the_next_model_step() {
    let provider = FakeProvider::new(vec![tool_reply(&[("call-1", r#"{"text":"one"}"#)])]);
    let (mut agent, worker) = steered_agent(&provider);
    worker.admit(plain(0, "go"));
    let (report, events) = run_steered(
        &mut agent,
        &worker,
        &CancellationToken::new(),
        |event, worker, _| {
            if matches!(event, UiEvent::ToolStarted { .. }) {
                worker.admit(with_skill(1, "$review this"));
            }
        },
    )
    .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(provider.requests().len(), 1);
    assert!(applied(&events).is_empty());
    assert_eq!(agent.history.len(), 3);
    let next = worker.take_next().unwrap();
    assert!(next.is_continuation());
    assert_eq!(next.text, "$review this");
}

#[tokio::test]
async fn an_explicit_cancel_stops_the_turn_and_leaves_the_steer_for_a_continuation() {
    let provider = FakeProvider::new(vec![
        streaming("partial"),
        text_reply("Kept going."),
        text_reply("Third."),
    ]);
    let (log, entries) = MemoryLog::shared();
    let (agent, worker) = steered_agent(&provider);
    let mut agent = logged(agent, log);
    worker.admit(plain(0, "go"));
    let (report, events) = run_steered(
        &mut agent,
        &worker,
        &CancellationToken::new(),
        |event, worker, cancel| {
            if matches!(event, UiEvent::AssistantText { .. }) {
                worker.admit(plain(1, "keep going"));
                worker.request_cancel();
                cancel.cancel();
            }
        },
    )
    .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert!(applied(&events).is_empty());
    let (report, _) =
        run_steered(&mut agent, &worker, &CancellationToken::new(), |_, _, _| {}).await;
    assert_eq!(report.final_text, "Kept going.");
    assert_eq!(
        provider.requests()[1].messages,
        [
            ChatMessage::user("go"),
            assistant("partial"),
            ChatMessage::user(steering_message("keep going")),
        ]
    );
    let users: Vec<String> = entries
        .lock()
        .unwrap()
        .iter()
        .filter_map(|entry| match entry {
            Logged::Turn { user, .. } => Some(user.clone()),
            Logged::Compaction { .. } => None,
        })
        .collect();
    assert_eq!(users, ["go", "keep going"]);
    worker.admit(plain(2, "third"));
    run_steered(&mut agent, &worker, &CancellationToken::new(), |_, _, _| {}).await;
    assert_eq!(
        provider.requests()[2].messages[2..],
        [
            ChatMessage::user("keep going"),
            assistant("Kept going."),
            ChatMessage::user("third"),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn an_interrupted_turn_keeps_steering_typed_right_after_a_tool_result() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-done", r#"{"text":"one"}"#)]),
        tool_reply(&[("call-active", r#"{"hang":true}"#)]),
    ]);
    let (log, entries) = MemoryLog::shared();
    let (agent, worker) = steered_agent(&provider);
    let mut agent = logged(agent, log);
    worker.admit(plain(0, "work"));
    let (report, events) = run_steered(
        &mut agent,
        &worker,
        &CancellationToken::new(),
        |event, worker, cancel| match event {
            UiEvent::ToolStarted { call_id, .. } if call_id.as_str() == "call-done" => {
                worker.admit(plain(1, "check the tests too"));
            }
            UiEvent::ToolStarted { call_id, .. } if call_id.as_str() == "call-active" => {
                worker.request_cancel();
                cancel.cancel();
            }
            _ => {}
        },
    )
    .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(applied(&events), [(1, "check the tests too")]);
    assert_eq!(
        agent.history,
        [
            ChatMessage::user("work"),
            calling("call-done", r#"{"text":"one"}"#),
            tool_message(
                "call-done",
                r#"echo {"text":"one"}"#,
                ToolResultStatus::Success
            ),
            ChatMessage::restored_steering("check the tests too"),
        ]
    );
    let turn = history_turn(&agent.history, 0, agent.history.len());
    assert_eq!(turn.steps.len(), 1);
    let steering: Vec<(&str, usize)> = turn
        .steering()
        .map(|entry| (entry.text, entry.after_tool_step_count))
        .collect();
    assert_eq!(steering, [("check the tests too", 1)]);
    let entries = entries.lock().unwrap();
    let Logged::Turn {
        steps,
        steering,
        end,
        ..
    } = &entries[0]
    else {
        panic!("expected a logged turn");
    };
    assert_eq!(steps.len(), 1);
    assert_eq!(steering, &["check the tests too||1"]);
    assert_eq!(end, r#"Cancelled """#);
}

#[tokio::test]
async fn a_steered_reply_is_logged_with_its_partial_text_as_the_steering_prefix() {
    let provider = FakeProvider::new(vec![streaming("Looking"), text_reply("Done.")]);
    let (log, entries) = MemoryLog::shared();
    let (agent, worker) = steered_agent(&provider);
    let mut agent = logged(agent, log);
    worker.admit(plain(0, "go"));
    let mut sent = false;
    run_steered(
        &mut agent,
        &worker,
        &CancellationToken::new(),
        |event, worker, _| {
            if matches!(event, UiEvent::AssistantText { .. }) && !sent {
                sent = true;
                worker.admit(plain(1, "use the new API"));
            }
        },
    )
    .await;
    assert_eq!(
        entries.lock().unwrap()[0],
        Logged::Turn {
            user: "go".to_owned(),
            steps: Vec::new(),
            steering: vec!["use the new API|Looking|0".to_owned()],
            end: r#"replied "Done." replay=false"#.to_owned(),
        }
    );
}

#[tokio::test]
async fn later_turns_see_earlier_steering_without_the_wrapper() {
    let provider = FakeProvider::new(vec![
        streaming("Looking"),
        text_reply("Done."),
        text_reply("Next."),
    ]);
    let (mut agent, worker) = steered_agent(&provider);
    worker.admit(plain(0, "go"));
    let mut sent = false;
    run_steered(
        &mut agent,
        &worker,
        &CancellationToken::new(),
        |event, worker, _| {
            if matches!(event, UiEvent::AssistantText { .. }) && !sent {
                sent = true;
                worker.admit(plain(1, "use the new API"));
            }
        },
    )
    .await;
    worker.admit(plain(2, "and now the docs"));
    run_steered(&mut agent, &worker, &CancellationToken::new(), |_, _, _| {}).await;
    assert_eq!(
        provider.requests()[2].messages,
        [
            ChatMessage::user("go"),
            assistant("Looking"),
            ChatMessage::restored_steering("use the new API"),
            assistant("Done."),
            ChatMessage::user("and now the docs"),
        ]
    );
}

#[tokio::test]
async fn an_explicit_cancel_of_the_turn_token_wins_over_steering_admitted_before_it() {
    let provider = FakeProvider::new(vec![streaming("partial")]);
    let (mut agent, worker) = steered_agent(&provider);
    worker.admit(plain(0, "go"));
    let (report, events) = run_steered(
        &mut agent,
        &worker,
        &CancellationToken::new(),
        |event, worker, cancel| {
            if matches!(event, UiEvent::AssistantText { .. }) {
                worker.admit(plain(1, "keep going"));
                cancel.cancel();
            }
        },
    )
    .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert!(applied(&events).is_empty());
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(
        agent.history,
        [ChatMessage::user("go"), assistant("partial")]
    );
    let next = worker.take_next().unwrap();
    assert!(next.is_continuation());
    assert_eq!(next.text, "keep going");
}

fn limited_to_one_step(provider: &Arc<FakeProvider>) -> (Agent, Arc<WorkerRuntime>) {
    let (mut agent, worker) = steered_agent(provider);
    agent.set_config(AgentConfig {
        step_limit: 1,
        ..config()
    });
    (agent, worker)
}

#[tokio::test]
async fn steering_that_no_model_step_is_left_to_answer_waits_for_a_continuation() {
    let reply_then_steer = || {
        Script::Reply(
            vec![StreamEvent::TextDelta {
                text: "partial".to_owned(),
            }],
            completion(Some("partial"), Vec::new(), FinishReason::Stop),
        )
    };
    for script in [streaming("partial"), reply_then_steer()] {
        let provider = FakeProvider::new(vec![script]);
        let (mut agent, worker) = limited_to_one_step(&provider);
        worker.admit(plain(0, "go"));
        let mut sent = false;
        let (report, events) = run_steered(
            &mut agent,
            &worker,
            &CancellationToken::new(),
            |event, worker, _| {
                if matches!(event, UiEvent::AssistantText { .. }) && !sent {
                    sent = true;
                    worker.admit(plain(1, "keep going"));
                }
            },
        )
        .await;
        assert_eq!(report.failure, Some(TurnFailure::StepLimitReached));
        assert!(applied(&events).is_empty());
        assert_eq!(provider.requests().len(), 1);
        assert_eq!(
            agent.history,
            [
                ChatMessage::user("go"),
                assistant("partial"),
                assistant(STEP_LIMIT_NOTICE),
            ]
        );
        let next = worker.take_next().unwrap();
        assert!(next.is_continuation());
        assert_eq!(next.text, "keep going");
    }
}

#[tokio::test]
async fn steering_after_a_cut_reply_is_taken_when_another_step_is_left() {
    let provider = FakeProvider::new(vec![
        Script::Reply(
            vec![StreamEvent::TextDelta {
                text: "partial".to_owned(),
            }],
            completion(Some("partial"), Vec::new(), FinishReason::Stop),
        ),
        text_reply("Steered."),
    ]);
    let (mut agent, worker) = steered_agent(&provider);
    agent.set_config(AgentConfig {
        step_limit: 2,
        ..config()
    });
    worker.admit(plain(0, "go"));
    let mut sent = false;
    let (report, events) = run_steered(
        &mut agent,
        &worker,
        &CancellationToken::new(),
        |event, worker, _| {
            if matches!(event, UiEvent::AssistantText { .. }) && !sent {
                sent = true;
                worker.admit(plain(1, "keep going"));
            }
        },
    )
    .await;
    assert_eq!(report.final_text, "Steered.");
    assert_eq!(applied(&events), [(1, "keep going")]);
    assert_eq!(
        provider.requests()[1].messages,
        [
            ChatMessage::user("go"),
            assistant("partial"),
            ChatMessage::user(steering_message("keep going")),
        ]
    );
}
