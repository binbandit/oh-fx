use ofx_contract::{
    HistoryCut, HistoryTurn, RecoveryPoint, RecoveryProgress, RestoredHistory, TurnEnd,
};

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Logged {
    Turn {
        user: String,
        steps: Vec<String>,
        steering: Vec<String>,
        end: String,
    },
    Compaction {
        checkpoint: String,
        cut: HistoryCut,
        user: Option<String>,
        steps: Vec<String>,
    },
    Recovery {
        user: String,
        steps: Vec<String>,
        progress: RecoveryProgress,
        consumed_attempts: usize,
        fast_mode: bool,
    },
    RecoveryCleared,
}

#[derive(Default)]
pub(super) struct MemoryLog {
    pub(super) entries: Arc<Mutex<Vec<Logged>>>,
    pub(super) failing: Option<&'static str>,
    pub(super) blocked: Option<&'static str>,
    pub(super) refused_checkpoint: Option<&'static str>,
    pub(super) refused_turn: Option<&'static str>,
}

impl MemoryLog {
    pub(super) fn shared() -> (Box<Self>, Arc<Mutex<Vec<Logged>>>) {
        let log = Box::<Self>::default();
        let entries = Arc::clone(&log.entries);
        (log, entries)
    }

    pub(super) fn failing(code: &'static str) -> Self {
        Self {
            failing: Some(code),
            ..Self::default()
        }
    }

    fn outcome(&self) -> Result<(), LogFailure> {
        match self.failing {
            Some(code) => Err(LogFailure {
                code: code.to_owned(),
            }),
            None => Ok(()),
        }
    }
}

fn described_steps(turn: &HistoryTurn<'_>) -> Vec<String> {
    turn.steps
        .iter()
        .map(|step| {
            let calls: Vec<&str> = step
                .tool_calls
                .iter()
                .map(|call| call.id.as_str())
                .collect();
            let results: Vec<String> = step
                .tool_results
                .iter()
                .map(|result| {
                    let raw = if result.output_bytes == result.output.len() {
                        String::new()
                    } else {
                        format!(" raw={}", result.output_bytes)
                    };
                    let whole = if result.model_view_covers_full_file {
                        " whole"
                    } else {
                        ""
                    };
                    format!(
                        "{}={}:{:?}{raw}{whole}",
                        result.call_id, result.output, result.status
                    )
                })
                .collect();
            format!(
                "{:?} replay={} calls={calls:?} results={results:?}",
                step.assistant,
                step.provider_replay.is_some()
            )
        })
        .collect()
}

fn described_end(end: TurnEnd<'_>) -> String {
    match end {
        TurnEnd::Replied {
            text,
            provider_replay,
        } => format!("replied {text:?} replay={}", provider_replay.is_some()),
        TurnEnd::Stopped { reason, partial } => format!("{reason:?} {partial:?}"),
    }
}

impl ConversationLog for MemoryLog {
    fn require_writable(&self) -> Result<(), LogFailure> {
        match self.blocked {
            Some(code) => Err(LogFailure {
                code: code.to_owned(),
            }),
            None => Ok(()),
        }
    }

    fn record_turn(&mut self, turn: &HistoryTurn<'_>) -> Result<(), LogFailure> {
        self.outcome()?;
        if let Some(code) = self.refused_turn.take() {
            return Err(LogFailure {
                code: code.to_owned(),
            });
        }
        self.entries.lock().unwrap().push(Logged::Turn {
            user: turn.user.to_owned(),
            steps: described_steps(turn),
            steering: turn
                .steering
                .iter()
                .map(|entry| {
                    format!(
                        "{}|{}|{}",
                        entry.text, entry.assistant_prefix, entry.after_tool_step_count
                    )
                })
                .collect(),
            end: described_end(turn.end),
        });
        Ok(())
    }

    fn record_compaction(
        &mut self,
        checkpoint: &str,
        cut: HistoryCut,
        active: Option<&HistoryTurn<'_>>,
    ) -> Result<(), LogFailure> {
        self.outcome()?;
        if let Some(code) = self.refused_checkpoint {
            return Err(LogFailure {
                code: code.to_owned(),
            });
        }
        if let Some(active) = active {
            assert_eq!(
                active.end,
                TurnEnd::Replied {
                    text: "",
                    provider_replay: None
                }
            );
        }
        self.entries.lock().unwrap().push(Logged::Compaction {
            checkpoint: checkpoint.to_owned(),
            cut,
            user: active.map(|active| active.user.to_owned()),
            steps: active.map(described_steps).unwrap_or_default(),
        });
        Ok(())
    }

    fn record_recovery(&self, point: &RecoveryPoint<'_>) -> Result<(), LogFailure> {
        self.outcome()?;
        self.entries.lock().unwrap().push(Logged::Recovery {
            user: point.turn.user.to_owned(),
            steps: described_steps(&point.turn),
            progress: point.progress,
            consumed_attempts: point.consumed_attempts,
            fast_mode: point.fast_mode,
        });
        Ok(())
    }

    fn clear_recovery(&self) -> Result<(), LogFailure> {
        self.entries.lock().unwrap().push(Logged::RecoveryCleared);
        Ok(())
    }
}

fn logged_turn(user: &str, steps: &[&str], end: &str) -> Logged {
    Logged::Turn {
        user: user.to_owned(),
        steps: steps.iter().map(|step| (*step).to_owned()).collect(),
        steering: Vec::new(),
        end: end.to_owned(),
    }
}

pub(super) fn logged(mut agent: Agent, log: Box<dyn ConversationLog>) -> Agent {
    agent.attach_session("abcdefghijkl".to_owned(), log);
    agent
}

fn logging_agent(provider: &Arc<FakeProvider>) -> (Agent, Arc<Mutex<Vec<Logged>>>) {
    let (log, entries) = MemoryLog::shared();
    let shared: Arc<FakeProvider> = Arc::clone(provider);
    (logged(new_agent(shared, vec![echo_tool()]), log), entries)
}

#[tokio::test]
async fn finished_turns_are_logged_with_their_tool_steps_and_reply() {
    let provider = FakeProvider::new(vec![
        with_replay(
            tool_reply(&[("call-1", "{}"), ("call-2", r#"{"fail":1}"#)]),
            "p1",
        ),
        with_replay(text_reply("done"), "p2"),
    ]);
    let (mut agent, entries) = logging_agent(&provider);
    let (report, _) = run(&mut agent, "read").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        *entries.lock().unwrap(),
        [logged_turn(
            "read",
            &[
                r#""" replay=true calls=["call-1", "call-2"] results=["call-1=echo {}:Success", "call-2=echo failed:Failure"]"#
            ],
            r#"replied "done" replay=true"#
        )]
    );
    assert_eq!(
        provider.sessions(),
        [
            Some("abcdefghijkl".to_owned()),
            Some("abcdefghijkl".to_owned())
        ]
    );
}

#[tokio::test]
async fn requests_without_a_session_carry_no_session_id() {
    let provider = FakeProvider::new(vec![text_reply("hello")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    run(&mut agent, "hi").await;
    assert_eq!(provider.sessions(), [None]);
}

#[tokio::test]
async fn interrupted_and_failed_turns_are_logged_as_upstream_saves_them() {
    let partial = |text: &str| {
        vec![StreamEvent::TextDelta {
            text: text.to_owned(),
        }]
    };
    let refused = || failure(ProviderErrorKind::Unauthorized, "unauthorized");
    let provider = FakeProvider::new(vec![
        Script::Fail(partial("half"), ProviderError::cancelled()),
        Script::Fail(Vec::new(), refused()),
        Script::Fail(
            partial("Partial answer"),
            failure(ProviderErrorKind::TransportInterrupted, "ReadFailed"),
        ),
        tool_reply(&[("call-1", "{}")]),
        Script::Fail(Vec::new(), refused()),
        tool_reply(&[("call-2", "{}")]),
        Script::Fail(partial(" \n"), refused()),
    ]);
    let (mut agent, entries) = logging_agent(&provider);
    for prompt in ["stop", "empty", "partial", "worked", "blank"] {
        run(&mut agent, prompt).await;
    }
    assert_eq!(
        *entries.lock().unwrap(),
        [
            logged_turn("stop", &[], r#"Cancelled "half""#),
            logged_turn("empty", &[], r#"Failed """#),
            logged_turn("partial", &[], r#"Failed "Partial answer""#),
            logged_turn(
                "worked",
                &[r#""" replay=false calls=["call-1"] results=["call-1=echo {}:Success"]"#],
                r#"replied "" replay=false"#
            ),
            logged_turn(
                "blank",
                &[r#""" replay=false calls=["call-2"] results=["call-2=echo {}:Success"]"#],
                r#"replied "" replay=false"#
            ),
        ]
    );
    assert!(!agent.history.iter().any(|message| matches!(
        message,
        ChatMessage::Assistant { content: Some(text), .. } if text.trim().is_empty() && !text.is_empty()
    )));
}

#[tokio::test]
async fn step_limits_are_logged_as_the_notice_reply() {
    let provider = FakeProvider::new(vec![tool_reply(&[("call-1", "{}")])]);
    let (log, entries) = MemoryLog::shared();
    let agent = Agent::new(
        provider,
        vec![echo_tool()],
        Arc::new(FixedContext),
        Arc::new(ArgumentGate),
        AgentConfig {
            step_limit: 1,
            ..config()
        },
    );
    let mut agent = logged(agent, log);
    let (report, _) = run(&mut agent, "loop").await;
    assert_eq!(report.failure, Some(TurnFailure::StepLimitReached));
    assert_eq!(
        *entries.lock().unwrap(),
        [logged_turn(
            "loop",
            &[r#""" replay=false calls=["call-1"] results=["call-1=echo {}:Success"]"#],
            &format!("replied {STEP_LIMIT_NOTICE:?} replay=false")
        )]
    );
}

#[tokio::test]
async fn a_turn_that_cannot_be_saved_keeps_its_outcome_and_reports_the_log_error() {
    let provider = FakeProvider::new(vec![text_reply("hello")]);
    let mut agent = logged(
        new_agent(provider, Vec::new()),
        Box::new(MemoryLog::failing("SessionPersistenceUncertain")),
    );
    let (report, events) = run(&mut agent, "hi").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        report.failure.map(|failure| failure.code().to_owned()),
        Some("SessionPersistenceUncertain".to_owned())
    );
    assert!(matches!(
        events.last(),
        Some(UiEvent::TurnFinished {
            outcome: TurnOutcome::Completed,
            ..
        })
    ));
    assert_eq!(agent.history.len(), 2);
    let provider = FakeProvider::new(vec![Script::Fail(
        Vec::new(),
        failure(ProviderErrorKind::Unauthorized, "unauthorized"),
    )]);
    let mut agent = logged(
        new_agent(provider, Vec::new()),
        Box::new(MemoryLog::failing("SessionPersistenceUncertain")),
    );
    let (report, _) = run(&mut agent, "hi").await;
    assert_eq!(report.failure.unwrap().code(), "unauthorized");
}

#[tokio::test]
async fn a_log_that_refuses_writes_fails_the_turn_before_the_model_runs() {
    let provider = FakeProvider::new(vec![text_reply("never")]);
    let (mut log, entries) = MemoryLog::shared();
    log.blocked = Some("SessionCommitFailed");
    let mut agent = logged(new_agent(Arc::clone(&provider), Vec::new()), log);
    let (report, events) = run(&mut agent, "hi").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(report.failure.unwrap().code(), "SessionCommitFailed");
    assert!(matches!(events[0], UiEvent::TurnStarted { .. }));
    assert!(matches!(
        events.last(),
        Some(UiEvent::TurnFinished {
            outcome: TurnOutcome::Failed,
            ..
        })
    ));
    assert!(provider.requests().is_empty());
    assert!(entries.lock().unwrap().is_empty());
    assert!(agent.history.is_empty());
}

#[tokio::test]
async fn a_session_attached_later_names_requests_and_records_turns() {
    let provider = FakeProvider::new(vec![text_reply("one"), text_reply("two")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (log, entries) = MemoryLog::shared();
    agent.attach_session("session-1".to_owned(), log);
    run(&mut agent, "first").await;
    agent.detach_session();
    run(&mut agent, "second").await;
    assert_eq!(entries.lock().unwrap().len(), 1);
    assert_eq!(provider.sessions(), [Some("session-1".to_owned()), None]);
}

#[tokio::test]
async fn restored_history_is_sent_ahead_of_the_next_prompt() {
    let provider = FakeProvider::new(vec![text_reply("again")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let restored = vec![
        ChatMessage::user("first"),
        ChatMessage::Assistant {
            content: Some("one".to_owned()),
            tool_calls: Vec::new(),
            provider_replay: None,
        },
    ];
    agent.restore(RestoredHistory {
        checkpoint: Some("older work".to_owned()),
        messages: restored.clone(),
        turn_starts: vec![0],
    });
    assert_eq!(agent.turn_starts, [1]);
    assert_eq!(agent.compacted, None);
    run(&mut agent, "second").await;
    let messages = &provider.requests()[0].messages;
    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages[0],
        ChatMessage::user(
            "This session is being continued from earlier compacted context. The summary below covers the earlier portion of the conversation.\n\nolder work\n\nRecent conversation turns are preserved verbatim.\nContinue the conversation from where it left off without asking the user to repeat context. Resume directly."
        )
    );
    assert_eq!(messages[1..3], restored[..]);
    assert_eq!(messages[3], ChatMessage::user("second"));
    assert_eq!(agent.turn_starts, [1, 3]);

    let payload = Payload {
        turn_count: 1,
        ..Payload::default()
    };
    agent.restore(RestoredHistory {
        checkpoint: Some(crate::compactor::encode_checkpoint(&payload)),
        messages: Vec::new(),
        turn_starts: Vec::new(),
    });
    assert_eq!(agent.compacted, Some(payload));
    assert_eq!(agent.history.len(), 1);
    assert!(agent.turn_starts.is_empty());
    agent.restore(RestoredHistory::default());
    assert!(agent.history.is_empty());
    assert_eq!(agent.compacted, None);
}

#[tokio::test]
async fn results_cut_for_the_model_are_logged_with_the_length_the_tool_returned() {
    let arguments = format!(r#"{{"value":"{}"}}"#, "x".repeat(70_000));
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", &arguments), ("call-2", "{}")]),
        text_reply("done"),
    ]);
    let (mut agent, entries) = logging_agent(&provider);
    let (report, _) = run(&mut agent, "big").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let entries = entries.lock().unwrap().clone();
    let Logged::Turn { steps, .. } = &entries[0] else {
        panic!("{entries:?}");
    };
    let raw = format!("echo {arguments}").len();
    assert!(raw > DEFAULT_MAX_TOOL_RESULT_BYTES);
    assert!(
        steps[0].contains(&format!(
            ":Success raw={raw}\", \"call-2=echo {{}}:Success\"]"
        )),
        "{}",
        &steps[0][steps[0].len() - 200..]
    );
}

#[tokio::test]
async fn a_whole_file_view_is_logged_only_when_the_model_saw_all_of_it() {
    let large = format!(r#"{{"whole":"{}"}}"#, "x".repeat(70_000));
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"whole":1}"#),
            ("call-2", &large),
            ("call-3", "{}"),
        ]),
        text_reply("done"),
    ]);
    let (mut agent, entries) = logging_agent(&provider);
    let (report, _) = run(&mut agent, "read").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let entries = entries.lock().unwrap().clone();
    let Logged::Turn { steps, .. } = &entries[0] else {
        panic!("{entries:?}");
    };
    assert!(
        steps[0].contains(r#""call-1=echo {\"whole\":1}:Success whole""#),
        "{}",
        &steps[0][..200]
    );
    assert!(
        !steps[0].contains(":Success raw=70016 whole"),
        "truncated output kept its view"
    );
    assert!(
        steps[0].ends_with(r#""call-3=echo {}:Success"]"#),
        "{}",
        &steps[0][steps[0].len() - 80..]
    );
}
