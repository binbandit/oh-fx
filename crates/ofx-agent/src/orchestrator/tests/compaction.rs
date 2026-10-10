use ofx_contract::{
    ApprovalAnswer, ApprovalDecision, CompactionActivity, CompactionEnd, HistoryCut,
};

use super::*;
use crate::compactor::CompactionError;

pub(super) async fn chat(agent: &mut Agent, turns: usize) {
    for turn in 1..=turns {
        let (report, _) = run(agent, &format!("question {turn}")).await;
        assert_eq!(report.outcome, TurnOutcome::Completed);
    }
}

pub(super) fn chat_replies(turns: usize) -> Vec<Script> {
    (1..=turns)
        .map(|turn| text_reply(&format!("answer {turn}")))
        .collect()
}

fn user_text(message: &ChatMessage) -> &str {
    match message {
        ChatMessage::User { content, .. } => content,
        _ => "",
    }
}

#[tokio::test]
async fn manual_compaction_keeps_the_newest_turns_and_replaces_the_rest_with_a_checkpoint() {
    let mut scripts = chat_replies(6);
    scripts.push(text_reply("answer 7"));
    let provider = FakeProvider::new(scripts);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    chat(&mut agent, 6).await;

    let mut summarizing = 0;
    assert_eq!(
        agent
            .compact(&mut || summarizing += 1, &CancellationToken::new())
            .await,
        Ok(Compaction::Compacted)
    );
    assert_eq!(summarizing, 1);
    assert_eq!(provider.requests().len(), 6);
    assert_eq!(agent.last_assistant_reply().as_deref(), Some("answer 6"));

    run(&mut agent, "question 7").await;
    let requests = provider.requests();
    let messages = &requests[6].messages;
    let checkpoint = user_text(&messages[0]);
    assert!(checkpoint.starts_with("<compacted_conversation>\n"));
    assert!(
        checkpoint.contains("Turn 1\nUser 1:\nquestion 1\n\nAssistant 1, final reply:\nanswer 1\n")
    );
    assert!(checkpoint.contains("Turn 2\nUser 2:\nquestion 2\n"));
    assert!(!checkpoint.contains("question 3"));
    assert_eq!(user_text(&messages[1]), "question 3");
    assert_eq!(messages.len(), 1 + 4 * 2 + 1);
    assert_eq!(user_text(&messages[9]), "question 7");
}

#[tokio::test]
async fn manual_compaction_logs_its_checkpoint_without_an_active_turn() {
    let provider = FakeProvider::new(chat_replies(6));
    let (log, entries) = turn_log::MemoryLog::shared();
    let mut agent = turn_log::logged(new_agent(Arc::clone(&provider), Vec::new()), log);
    chat(&mut agent, 6).await;
    assert_eq!(
        agent.compact(&mut || {}, &CancellationToken::new()).await,
        Ok(Compaction::Compacted)
    );
    let entries = entries.lock().unwrap().clone();
    assert_eq!(entries.len(), 7);
    let turn_log::Logged::Compaction {
        checkpoint,
        cut,
        user,
        steps,
    } = &entries[6]
    else {
        panic!("{entries:?}");
    };
    assert_eq!(
        *cut,
        HistoryCut {
            turns: 2,
            tool_steps: 0,
            ..HistoryCut::default()
        }
    );
    assert_eq!(*user, None);
    assert!(steps.is_empty());
    let (text, payload) = crate::compactor::restore_checkpoint(checkpoint);
    assert!(payload.is_some());
    assert_eq!(text, user_text(&agent.history[0]));
}

#[tokio::test]
async fn a_checkpoint_counts_only_the_turns_the_log_saved() {
    let mut scripts = chat_replies(6);
    scripts.push(text_reply("answer 7"));
    let provider = FakeProvider::new(scripts);
    let entries = Arc::new(Mutex::new(Vec::new()));
    let log = Box::new(turn_log::MemoryLog {
        entries: Arc::clone(&entries),
        refused_turn: Some("Io(Other)"),
        ..turn_log::MemoryLog::default()
    });
    let mut agent = turn_log::logged(new_agent(Arc::clone(&provider), Vec::new()), log);
    chat(&mut agent, 6).await;
    assert_eq!(
        agent.compact(&mut || {}, &CancellationToken::new()).await,
        Ok(Compaction::Compacted)
    );
    let entries = entries.lock().unwrap().clone();
    assert_eq!(entries.len(), 6);
    let turn_log::Logged::Compaction { cut, .. } = &entries[5] else {
        panic!("{entries:?}");
    };
    assert_eq!(
        *cut,
        HistoryCut {
            turns: 1,
            tool_steps: 0,
            ..HistoryCut::default()
        }
    );
    run(&mut agent, "question 7").await;
    let requests = provider.requests();
    assert_eq!(user_text(&requests[6].messages[1]), "question 3");
}

#[tokio::test]
async fn a_manual_compaction_that_cannot_be_saved_keeps_the_whole_history() {
    let mut scripts = chat_replies(6);
    scripts.push(text_reply("answer 7"));
    let provider = FakeProvider::new(scripts);
    let log = Box::new(turn_log::MemoryLog {
        refused_checkpoint: Some("SessionCommitFailed"),
        ..turn_log::MemoryLog::default()
    });
    let mut agent = turn_log::logged(new_agent(Arc::clone(&provider), Vec::new()), log);
    chat(&mut agent, 6).await;
    assert_eq!(
        agent.compact(&mut || {}, &CancellationToken::new()).await,
        Err(CompactionError::NotSaved)
    );
    assert!(agent.compacted.is_none());
    run(&mut agent, "question 7").await;
    let requests = provider.requests();
    assert_eq!(user_text(&requests[6].messages[0]), "question 1");
    assert_eq!(requests[6].messages.len(), 6 * 2 + 1);
}

#[tokio::test]
async fn manual_compaction_asks_the_conversations_model_for_notes_on_tool_work() {
    let mut scripts = vec![
        tool_reply(&[("call-1", r#"{"value":"notes.md"}"#)]),
        text_reply("Read the notes."),
    ];
    scripts.extend(chat_replies(4));
    scripts.push(text_reply(
        "Turn 1\nIn between: Echoed the notes.\nT1: echoed notes.md",
    ));
    scripts.push(text_reply("done"));
    let provider = FakeProvider::new(scripts);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    run(&mut agent, "read the notes").await;
    chat(&mut agent, 4).await;

    assert_eq!(
        agent.compact(&mut || {}, &CancellationToken::new()).await,
        Ok(Compaction::Compacted)
    );
    let requests = provider.requests();
    let summary = &requests[6];
    assert_eq!(summary.model, "test-model");
    assert_eq!(summary.instructions.len(), 1);
    assert!(summary.instructions[0].starts_with("You write compaction notes"));
    assert!(summary.tools.is_empty());
    assert_eq!(summary.max_output_tokens, Some(64));
    assert_eq!(summary.reasoning_effort, None);
    assert_eq!(summary.messages.len(), 1);
    let asked = user_text(&summary.messages[0]);
    assert!(asked.starts_with("[Turn 1]\n[User]\nread the notes\n"));
    assert!(asked.contains("[Tool call T1: echo]\n{\"value\":\"notes.md\"}\n"));

    run(&mut agent, "and now?").await;
    let requests = provider.requests();
    let checkpoint = user_text(&requests[7].messages[0]);
    assert!(checkpoint.contains("Assistant 1, in between:\nEchoed the notes.\n"));
    assert!(checkpoint.contains("Tools:\n  T1 echo notes.md (25 bytes): echoed notes.md\n"));
    assert_eq!(user_text(&requests[7].messages[1]), "question 1");
}

#[tokio::test]
async fn manual_compaction_keeps_approval_feedback_in_the_notes_on_its_step() {
    let mut scripts = vec![
        tool_reply(&[("call-1", r#"{"value":"outside"}"#)]),
        text_reply("Read the notes."),
    ];
    scripts.extend(chat_replies(4));
    scripts.push(text_reply(
        "Turn 1\nIn between: Echoed the notes; the user asked to read the tests next.\nT1: echoed outside",
    ));
    scripts.push(text_reply("done"));
    let provider = FakeProvider::new(scripts);
    let approvals = Approvals::default();
    let mut agent =
        new_agent(Arc::clone(&provider), vec![echo_tool()]).with_approvals(approvals.clone());
    let report = agent
        .run_turn(
            "read the notes",
            &mut |event| {
                if let UiEvent::ApprovalRequested { request, .. } = &event {
                    assert!(approvals.resolve(
                        request.id,
                        ApprovalAnswer {
                            decision: ApprovalDecision::Once,
                            feedback: Some("read the tests next".to_owned()),
                        }
                    ));
                }
            },
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    chat(&mut agent, 4).await;

    assert_eq!(
        agent.compact(&mut || {}, &CancellationToken::new()).await,
        Ok(Compaction::Compacted)
    );
    let requests = provider.requests();
    let asked = user_text(&requests[6].messages[0]);
    assert!(
        asked.contains("[From oh-fx, not the user]\nPermission feedback: read the tests next\n"),
        "{asked}"
    );

    run(&mut agent, "and now?").await;
    let requests = provider.requests();
    let checkpoint = user_text(&requests[7].messages[0]);
    assert!(
        checkpoint.contains("the user asked to read the tests next"),
        "{checkpoint}"
    );
    assert_eq!(user_text(&requests[7].messages[1]), "question 1");
}

#[tokio::test]
async fn history_turns_count_the_checkpoint_as_one_turn() {
    let provider = FakeProvider::new(chat_replies(6));
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    assert_eq!(agent.history_turns(), 0);
    chat(&mut agent, 6).await;
    assert_eq!(agent.history_turns(), 6);
    assert_eq!(
        agent.compact(&mut || {}, &CancellationToken::new()).await,
        Ok(Compaction::Compacted)
    );
    assert_eq!(agent.history_turns(), 5);
    agent.clear_history();
    assert_eq!(agent.history_turns(), 0);
    assert!(!agent.has_context_to_compact());
}

#[tokio::test]
async fn a_conversation_that_fits_is_left_alone() {
    let provider = FakeProvider::new(chat_replies(2));
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    assert!(!agent.has_context_to_compact());
    chat(&mut agent, 1).await;
    assert!(agent.has_context_to_compact());
    let mut summarizing = false;
    assert_eq!(
        agent
            .compact(&mut || summarizing = true, &CancellationToken::new())
            .await,
        Ok(Compaction::Unchanged)
    );
    assert!(!summarizing);
    run(&mut agent, "question 2").await;
    assert_eq!(provider.requests()[1].messages.len(), 3);
}

#[tokio::test]
async fn a_failed_summary_keeps_the_whole_history() {
    let mut scripts = vec![
        tool_reply(&[("call-1", r#"{"value":"notes.md"}"#)]),
        text_reply("Read the notes."),
    ];
    scripts.extend(chat_replies(4));
    scripts.push(Script::Fail(
        Vec::new(),
        failure(ProviderErrorKind::InvalidRequest, "BadRequest"),
    ));
    scripts.push(text_reply("done"));
    let provider = FakeProvider::new(scripts);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    run(&mut agent, "read the notes").await;
    chat(&mut agent, 4).await;

    assert_eq!(
        agent.compact(&mut || {}, &CancellationToken::new()).await,
        Err(CompactionError::ModelFailed)
    );
    run(&mut agent, "and now?").await;
    let requests = provider.requests();
    assert_eq!(user_text(&requests[7].messages[0]), "read the notes");
    assert_eq!(requests[7].messages.len(), 4 + 4 * 2 + 1);
}

#[tokio::test]
async fn a_cancelled_compaction_sends_nothing_and_keeps_the_history() {
    let provider = FakeProvider::new(chat_replies(6));
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    chat(&mut agent, 6).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        agent.compact(&mut || {}, &cancel).await,
        Err(CompactionError::Cancelled)
    );
    assert_eq!(provider.requests().len(), 6);
}

#[tokio::test]
async fn a_compaction_cancelled_once_its_summary_is_written_saves_no_checkpoint() {
    let provider = FakeProvider::new(chat_replies(6));
    let (log, entries) = turn_log::MemoryLog::shared();
    let mut agent = turn_log::logged(new_agent(Arc::clone(&provider), Vec::new()), log);
    chat(&mut agent, 6).await;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    assert_eq!(
        agent.compact(&mut || trigger.cancel(), &cancel).await,
        Err(CompactionError::Cancelled)
    );
    assert_eq!(provider.requests().len(), 6);
    let entries = entries.lock().unwrap().clone();
    assert_eq!(entries.len(), 6, "{entries:?}");
    assert!(
        entries
            .iter()
            .all(|entry| !matches!(entry, turn_log::Logged::Compaction { .. }))
    );
    assert!(agent.compacted.is_none());
    assert_eq!(user_text(&agent.history[0]), "question 1");
}

#[tokio::test]
async fn clearing_the_history_forgets_the_checkpoint() {
    let mut scripts = chat_replies(6);
    scripts.push(text_reply("fresh"));
    let provider = FakeProvider::new(scripts);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    chat(&mut agent, 6).await;
    assert_eq!(
        agent.compact(&mut || {}, &CancellationToken::new()).await,
        Ok(Compaction::Compacted)
    );
    agent.clear_history();
    run(&mut agent, "start over").await;
    let requests = provider.requests();
    assert_eq!(requests[6].messages, [ChatMessage::user("start over")]);
}

pub(super) struct Window {
    tokens: u32,
    lookups: AtomicUsize,
}

impl CapabilityResolver for Window {
    fn resolve<'a>(
        &'a self,
        _model: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, CapabilityLookup> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        let capabilities = ModelCapabilities {
            context_window: Some(self.tokens),
            ..ModelCapabilities::default()
        };
        Box::pin(async move { CapabilityLookup::Resolved(capabilities) })
    }
}

pub(super) fn windowed(
    provider: &Arc<FakeProvider>,
    tokens: u32,
    max_output_tokens: u32,
) -> (Agent, Arc<Window>) {
    windowed_with_tools(provider, tokens, max_output_tokens, vec![echo_tool()])
}

pub(super) fn windowed_with_tools(
    provider: &Arc<FakeProvider>,
    tokens: u32,
    max_output_tokens: u32,
    tools: Vec<Arc<dyn Tool>>,
) -> (Agent, Arc<Window>) {
    let window = Arc::new(Window {
        tokens,
        lookups: AtomicUsize::new(0),
    });
    let shared: Arc<FakeProvider> = Arc::clone(provider);
    let agent = Agent::new(
        shared,
        tools,
        Arc::new(FixedContext),
        Arc::new(ArgumentGate),
        AgentConfig {
            max_output_tokens: Some(max_output_tokens),
            ..config()
        },
    )
    .with_capability_resolver(Arc::clone(&window) as Arc<dyn CapabilityResolver>);
    (agent, window)
}

pub(super) fn unmetered(script: Script) -> Script {
    metered(script, None)
}

pub(super) fn metered(script: Script, input_tokens: Option<u64>) -> Script {
    match script {
        Script::Reply(stream, mut completion) => {
            completion.usage.input_tokens = input_tokens;
            Script::Reply(stream, completion)
        }
        other => other,
    }
}

pub(super) fn spoken_tool_reply(content: &str, id: &str, arguments: &str) -> Script {
    unmetered(Script::Reply(
        Vec::new(),
        completion(
            Some(content),
            vec![echo_call(id, arguments)],
            FinishReason::ToolCalls,
        ),
    ))
}

fn overflow(kind: ProviderErrorKind, detail: Option<&str>) -> Script {
    let mut error = failure(kind, "BadRequest");
    error.detail = detail.map(str::to_owned);
    Script::Fail(Vec::new(), error)
}

fn compaction_activity(events: &[UiEvent]) -> Vec<CompactionActivity> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::TurnCompaction { turn_id, activity } => {
                assert_eq!(*turn_id, TurnId::new(2));
                Some(*activity)
            }
            _ => None,
        })
        .collect()
}

const SHOWN_THEN_COMPACTED: [CompactionActivity; 3] = [
    CompactionActivity::Preparing,
    CompactionActivity::Summarizing,
    CompactionActivity::Compacted,
];

fn mentions(message: &ChatMessage, text: &str) -> bool {
    match message {
        ChatMessage::User { content, .. }
        | ChatMessage::System { content }
        | ChatMessage::Tool { content, .. } => content.contains(text),
        ChatMessage::Assistant { content, .. } => content
            .as_deref()
            .is_some_and(|content| content.contains(text)),
    }
}

#[tokio::test]
async fn a_request_past_the_compaction_point_compacts_older_turns_and_continues() {
    let big_reply = format!("AUTO_HISTORY_FINAL_SENTINEL {}", "h".repeat(150_000));
    let provider = FakeProvider::new(vec![
        spoken_tool_reply("Reading first.", "call-1", r#"{"value":"first.txt"}"#),
        unmetered(text_reply(&big_reply)),
        unmetered(text_reply(
            "Turn 1\nIn between: Finish after the verified read and return the result.\nT1: echoed first.txt",
        )),
        unmetered(text_reply("Automatic compaction complete.")),
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    let (first, events) = run(&mut agent, "AUTO_HISTORY_USER_SENTINEL").await;
    assert_eq!(first.outcome, TurnOutcome::Completed);
    assert!(compaction_activity(&events).is_empty());
    let (second, events) = run(&mut agent, "AUTO_RECENT_USER").await;
    assert_eq!(second.outcome, TurnOutcome::Completed);
    assert_eq!(compaction_activity(&events), SHOWN_THEN_COMPACTED);
    assert_eq!(second.final_text, "Automatic compaction complete.");

    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    let notes = &requests[2];
    assert_eq!(notes.instructions, requests[1].instructions);
    assert_eq!(notes.tools, requests[1].tools);
    assert_eq!(notes.messages.len(), 4 + 1 + 1);
    assert!(mentions(
        &notes.messages[5],
        "Write the compaction notes for the turns of the conversation above"
    ));
    assert!(mentions(&notes.messages[3], "AUTO_HISTORY_FINAL_SENTINEL"));

    let rebuilt = &requests[3];
    assert_eq!(rebuilt.messages.len(), 2);
    let checkpoint = user_text(&rebuilt.messages[0]);
    assert!(checkpoint.starts_with("<compacted_conversation>\n"));
    assert!(checkpoint.contains("User 1:\nAUTO_HISTORY_USER_SENTINEL\n"));
    assert!(checkpoint.contains("Finish after the verified read"));
    assert!(checkpoint.contains("AUTO_HISTORY_FINAL_SENTINEL"));
    assert!(checkpoint.contains("bytes left out here]"));
    assert!(checkpoint.len() < 100_000);
    assert_eq!(user_text(&rebuilt.messages[1]), "AUTO_RECENT_USER");
}

#[tokio::test]
async fn a_running_turn_compacts_its_finished_steps_and_keeps_its_prompt() {
    let big_step = format!("STEP_SENTINEL {}", "h".repeat(150_000));
    let provider = FakeProvider::new(vec![
        spoken_tool_reply(&big_step, "call-1", r#"{"value":"notes.md"}"#),
        unmetered(text_reply(
            "Turn in progress\nIn between: Read the notes.\nT1: echoed notes.md",
        )),
        unmetered(text_reply("done")),
        unmetered(text_reply("next")),
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    let (report, _) = run(&mut agent, "read the notes").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "done");

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].messages.len(), 3 + 1);
    assert!(mentions(
        &requests[1].messages[3],
        "Write the compaction notes for the turns of the conversation above"
    ));
    let rebuilt = &requests[2];
    assert_eq!(rebuilt.messages.len(), 2);
    let checkpoint = user_text(&rebuilt.messages[0]);
    assert!(checkpoint.contains("Turn in progress, whose first user message follows this:\n"));
    assert!(checkpoint.contains("Read the notes."));
    assert!(!checkpoint.contains("STEP_SENTINEL"));
    assert_eq!(user_text(&rebuilt.messages[1]), "read the notes");

    run(&mut agent, "and then?").await;
    let requests = provider.requests();
    let later = &requests[3].messages;
    assert_eq!(later.len(), 4);
    assert_eq!(user_text(&later[1]), "read the notes");
    assert_eq!(user_text(&later[3]), "and then?");
}

#[tokio::test]
async fn capabilities_are_looked_up_once_the_conversation_holds_something_to_compact() {
    let provider = FakeProvider::new(vec![
        unmetered(text_reply("hello")),
        unmetered(text_reply("again")),
    ]);
    let (mut agent, window) = windowed(&provider, 45_000, 64);
    run(&mut agent, "hi").await;
    assert_eq!(window.lookups.load(Ordering::SeqCst), 0);
    run(&mut agent, "hi again").await;
    assert_eq!(window.lookups.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn exact_input_usage_calibrates_the_next_estimate() {
    let provider = FakeProvider::new(vec![
        unmetered(text_reply("first answer")),
        metered(text_reply("short answer"), Some(40_000)),
        unmetered(text_reply("after compaction")),
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    run(&mut agent, "warm up").await;
    run(&mut agent, "short question").await;
    let (report, _) = run(&mut agent, "another short question").await;
    assert_eq!(report.final_text, "after compaction");

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].messages.len(), 2);
    let checkpoint = user_text(&requests[2].messages[0]);
    assert!(checkpoint.starts_with("<compacted_conversation>\n"));
    assert!(checkpoint.contains("User 1:\nwarm up\n"));
    assert!(checkpoint.contains("User 2:\nshort question\n"));
}

#[tokio::test]
async fn a_switched_provider_estimates_without_the_previous_provider_s_calibration() {
    let first = FakeProvider::new(vec![
        unmetered(text_reply("first answer")),
        metered(text_reply("short answer"), Some(40_000)),
    ]);
    let second = FakeProvider::new(vec![unmetered(text_reply("from the second provider"))]);
    let (mut agent, window) = windowed(&first, 45_000, 64);
    run(&mut agent, "warm up").await;
    run(&mut agent, "short question").await;
    let provider: Arc<FakeProvider> = Arc::clone(&second);
    agent.set_provider(provider, Some(window as Arc<dyn CapabilityResolver>));
    let (report, _) = run(&mut agent, "another short question").await;
    assert_eq!(report.final_text, "from the second provider");
    assert_eq!(first.requests().len(), 2);
    assert_eq!(second.requests()[0].messages.len(), 5);
}

#[tokio::test]
async fn a_context_overflow_compacts_once_and_retries() {
    let rejections = [
        overflow(
            ProviderErrorKind::InvalidRequest,
            Some(
                "API request failed · HTTP 400 · AI_APICallError: Your input exceeds the context window of this model.",
            ),
        ),
        overflow(
            ProviderErrorKind::InvalidRequest,
            Some("prompt is too long: 1077372 tokens > 1000000 maximum"),
        ),
        overflow(ProviderErrorKind::RequestTooLarge, None),
        overflow(
            ProviderErrorKind::ProviderError,
            Some(
                "provider error: context_length_exceeded: Your input exceeds the context window of this model.",
            ),
        ),
    ];
    for rejection in rejections {
        let provider = FakeProvider::new(vec![
            spoken_tool_reply("", "prior-read", r#"{"value":"PRIOR_TOOL_OUTPUT"}"#),
            unmetered(text_reply("PRIOR_ASSISTANT")),
            rejection,
            unmetered(text_reply(
                "Turn 1\nIn between: Retain the completed prior turn and continue from it.",
            )),
            unmetered(text_reply("RECOVERED")),
        ]);
        let (mut agent, _) = windowed(&provider, 128_000, 16_384);
        run(&mut agent, "PRIOR_USER").await;
        let (report, events) = run(&mut agent, "continue").await;
        assert_eq!(report.outcome, TurnOutcome::Completed);
        assert_eq!(report.final_text, "RECOVERED");
        assert_eq!(compaction_activity(&events), SHOWN_THEN_COMPACTED);

        let requests = provider.requests();
        assert_eq!(requests.len(), 5);
        let notes = &requests[3];
        assert!(notes.tools.is_empty());
        assert!(notes.instructions[0].starts_with("You write compaction notes"));
        assert_eq!(notes.messages.len(), 1);
        let rebuilt = &requests[4].messages;
        assert_eq!(rebuilt.len(), 2);
        assert!(user_text(&rebuilt[0]).contains("User 1:\nPRIOR_USER\n"));
        assert!(
            !rebuilt
                .iter()
                .any(|message| matches!(message, ChatMessage::Tool { .. }))
        );
        assert_eq!(user_text(&rebuilt[1]), "continue");
    }
}

#[tokio::test]
async fn a_second_context_overflow_fails_the_turn() {
    let detail = Some("maximum context length exceeded");
    let provider = FakeProvider::new(vec![
        spoken_tool_reply("", "prior-read", r#"{"value":"notes"}"#),
        unmetered(text_reply("prior assistant")),
        overflow(ProviderErrorKind::InvalidRequest, detail),
        unmetered(text_reply(
            "Turn 1\nIn between: Compact the prior turn once.",
        )),
        overflow(ProviderErrorKind::InvalidRequest, detail),
        unmetered(text_reply("MUST_NOT_RUN")),
    ]);
    let (mut agent, _) = windowed(&provider, 128_000, 16_384);
    run(&mut agent, "prior user").await;
    let (report, _) = run(&mut agent, "continue").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(
        report.failure.as_ref().map(TurnFailure::code),
        Some("BadRequest")
    );
    assert_eq!(provider.requests().len(), 5);
    assert_eq!(agent.history.len(), 1);
    assert!(user_text(&agent.history[0]).starts_with("<compacted_conversation>\n"));
    assert_eq!(agent.last_assistant_reply().as_deref(), None);
}

#[tokio::test]
async fn an_overflow_with_nothing_to_compact_is_a_provider_failure() {
    let provider = FakeProvider::new(vec![overflow(
        ProviderErrorKind::InvalidRequest,
        Some("input is too long"),
    )]);
    let (mut agent, window) = windowed(&provider, 128_000, 16_384);
    let (report, events) = run(&mut agent, "an enormous prompt").await;
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, UiEvent::TurnCompaction { .. }))
    );
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(
        report.failure.as_ref().map(TurnFailure::code),
        Some("BadRequest")
    );
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(window.lookups.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_request_that_still_does_not_fit_after_compaction_fails_with_context_capacity_exceeded() {
    let provider = FakeProvider::new(vec![unmetered(text_reply("small answer"))]);
    let (mut agent, _) = windowed(&provider, 2_000, 64);
    run(&mut agent, "small question").await;
    let (report, events) = run(&mut agent, &"x ".repeat(4_000)).await;
    assert_eq!(compaction_activity(&events), SHOWN_THEN_COMPACTED);
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(
        report.failure,
        Some(TurnFailure::Compaction(
            CompactionError::ContextCapacityExceeded
        ))
    );
    assert_eq!(
        report.failure.as_ref().map(TurnFailure::code),
        Some("ContextCapacityExceeded")
    );
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(agent.history.len(), 1);
    assert!(user_text(&agent.history[0]).contains("User 1:\nsmall question\n"));
}

#[tokio::test]
async fn cancelling_an_automatic_compaction_interrupts_the_turn_and_keeps_its_work() {
    let big_step = format!("STEP_SENTINEL {}", "h".repeat(150_000));
    let provider = FakeProvider::new(vec![
        spoken_tool_reply(&big_step, "call-1", r#"{"value":"notes.md"}"#),
        Script::WaitForCancel,
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let watched = Arc::clone(&provider);
    tokio::spawn(async move {
        while watched.requests().len() < 2 {
            tokio::task::yield_now().await;
        }
        trigger.cancel();
    });
    let mut events = Vec::new();
    let report = agent
        .run_turn("read the notes", &mut |event| events.push(event), &cancel)
        .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    let shown: Vec<CompactionActivity> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::TurnCompaction { activity, .. } => Some(*activity),
            _ => None,
        })
        .collect();
    assert_eq!(
        shown,
        [
            CompactionActivity::Preparing,
            CompactionActivity::Summarizing,
            CompactionActivity::Ended(CompactionEnd::Cancelled),
        ]
    );
    assert_eq!(report.failure, None);
    assert!(mentions(
        &provider.requests()[1].messages[3],
        "Write the compaction notes"
    ));
    assert_eq!(agent.history.len(), 3);
    assert_eq!(user_text(&agent.history[0]), "read the notes");
    assert!(mentions(&agent.history[1], "STEP_SENTINEL"));
}

#[tokio::test]
async fn an_automatic_compaction_cancelled_once_its_summary_is_written_interrupts_the_turn_unsaved()
{
    let provider = FakeProvider::new(vec![unmetered(text_reply("small answer"))]);
    let (agent, _) = windowed(&provider, 2_000, 64);
    let (log, entries) = turn_log::MemoryLog::shared();
    let mut agent = turn_log::logged(agent, log);
    run(&mut agent, "small question").await;
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    let report = agent
        .run_turn(
            &"x ".repeat(4_000),
            &mut |event| {
                if let UiEvent::TurnCompaction {
                    activity: CompactionActivity::Summarizing,
                    ..
                } = event
                {
                    cancel.cancel();
                }
                events.push(event);
            },
            &cancel,
        )
        .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(report.failure, None);
    assert_eq!(
        compaction_activity(&events),
        [
            CompactionActivity::Preparing,
            CompactionActivity::Summarizing,
            CompactionActivity::Ended(CompactionEnd::Cancelled),
        ]
    );
    assert_eq!(provider.requests().len(), 1);
    let entries = entries.lock().unwrap().clone();
    assert!(
        entries
            .iter()
            .all(|entry| !matches!(entry, turn_log::Logged::Compaction { .. })),
        "{entries:?}"
    );
    assert!(agent.compacted.is_none());
    assert_eq!(user_text(&agent.history[0]), "small question");
}

#[tokio::test]
async fn a_failed_automatic_compaction_fails_the_turn_with_its_error() {
    let big_step = format!("STEP_SENTINEL {}", "h".repeat(150_000));
    let provider = FakeProvider::new(vec![
        spoken_tool_reply(&big_step, "call-1", r#"{"value":"notes.md"}"#),
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::InvalidRequest, "BadRequest"),
        ),
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::InvalidRequest, "BadRequest"),
        ),
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    let (report, events) = run(&mut agent, "read the notes").await;
    let shown: Vec<CompactionActivity> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::TurnCompaction { activity, .. } => Some(*activity),
            _ => None,
        })
        .collect();
    assert_eq!(
        shown,
        [
            CompactionActivity::Preparing,
            CompactionActivity::Summarizing,
            CompactionActivity::Ended(CompactionEnd::Failed),
        ]
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].messages.len(), 1);
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(
        report.failure,
        Some(TurnFailure::Compaction(CompactionError::ModelFailed))
    );
    assert_eq!(agent.history.len(), 3);
}

#[tokio::test]
async fn a_turn_that_fails_after_compacting_its_steps_keeps_its_prompt_after_the_checkpoint() {
    let big_step = format!("STEP_SENTINEL {}", "h".repeat(150_000));
    let provider = FakeProvider::new(vec![
        spoken_tool_reply(&big_step, "call-1", r#"{"value":"notes.md"}"#),
        unmetered(text_reply(
            "Turn in progress\nIn between: Read the notes.\nT1: echoed notes.md",
        )),
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::InvalidRequest, "BadRequest"),
        ),
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    let (report, _) = run(&mut agent, "read the notes").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(agent.history.len(), 2);
    assert!(user_text(&agent.history[0]).contains("Turn in progress"));
    assert_eq!(user_text(&agent.history[1]), "read the notes");
    assert_eq!(agent.turn_starts, [1]);
}

#[tokio::test]
async fn compacting_every_step_keeps_the_summary_prompt_that_follows_them() {
    let large = format!(r#"{{"value":"{}"}}"#, "x".repeat(20_000));
    let provider = FakeProvider::new(vec![
        spoken_tool_reply("", "call-1", r#"{"value":"small"}"#),
        spoken_tool_reply("", "call-2", &large),
        metered(text_reply(""), Some(40_000)),
        unmetered(text_reply(
            "Turn in progress\nIn between: Did the work.\nT1: echoed small\nT2: echoed large",
        )),
        unmetered(text_reply("Summary of the work.")),
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    let (report, _) = run(&mut agent, "do the work").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "Summary of the work.");

    let requests = provider.requests();
    assert_eq!(requests.len(), 5);
    let rebuilt = &requests[4].messages;
    assert_eq!(rebuilt.len(), 3);
    assert!(
        user_text(&rebuilt[0])
            .contains("Turn in progress, whose first user message follows this:\n")
    );
    assert_eq!(user_text(&rebuilt[1]), "do the work");
    assert_eq!(
        rebuilt[2],
        ChatMessage::user("Summarize what you just did.")
    );
}

#[tokio::test(start_paused = true)]
async fn a_measured_request_is_sent_as_measured_and_a_retry_builds_it_again() {
    let provider = FakeProvider::new(vec![
        unmetered(text_reply("hello")),
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::ServerError, "ServerError"),
        ),
        unmetered(text_reply("again")),
    ]);
    let (mut agent, _) = windowed(&provider, 45_000, 64);
    run(&mut agent, "hi").await;
    assert!(provider.bodies().is_empty());
    let (report, _) = run(&mut agent, "hi again").await;
    assert_eq!(report.final_text, "again");
    assert_eq!(provider.requests().len(), 3);
    let bodies = provider.bodies();
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("hi again"));
}

#[tokio::test]
async fn other_provider_failures_are_not_overflows() {
    for rejection in [
        overflow(
            ProviderErrorKind::ProviderError,
            Some("provider error: invalid_prompt: the prompt was rejected"),
        ),
        overflow(
            ProviderErrorKind::InvalidRequest,
            Some("API request failed · HTTP 400 · unknown parameter"),
        ),
    ] {
        let provider = FakeProvider::new(vec![
            spoken_tool_reply("", "prior-read", r#"{"value":"notes"}"#),
            unmetered(text_reply("prior assistant")),
            rejection,
        ]);
        let (mut agent, _) = windowed(&provider, 128_000, 16_384);
        run(&mut agent, "prior user").await;
        let (report, _) = run(&mut agent, "continue").await;
        assert_eq!(report.outcome, TurnOutcome::Failed);
        assert_eq!(provider.requests().len(), 3);
    }
}

#[tokio::test]
async fn a_mid_turn_compaction_logs_its_checkpoint_and_the_steps_it_covers() {
    let big_step = format!("STEP_SENTINEL {}", "h".repeat(150_000));
    let provider = FakeProvider::new(vec![
        spoken_tool_reply(&big_step, "call-1", r#"{"value":"notes.md"}"#),
        unmetered(text_reply(
            "Turn in progress\nIn between: Read the notes.\nT1: echoed notes.md",
        )),
        unmetered(text_reply("done")),
    ]);
    let (agent, _) = windowed(&provider, 45_000, 64);
    let (log, entries) = turn_log::MemoryLog::shared();
    let mut agent = turn_log::logged(agent, log);
    let (report, _) = run(&mut agent, "read the notes").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let entries = entries.lock().unwrap().clone();
    assert_eq!(entries.len(), 2);
    let turn_log::Logged::Compaction {
        checkpoint,
        cut,
        user,
        steps,
    } = &entries[0]
    else {
        panic!("{entries:?}");
    };
    assert_eq!(
        *cut,
        HistoryCut {
            turns: 0,
            tool_steps: 1,
            ..HistoryCut::default()
        }
    );
    assert_eq!(user.as_deref(), Some("read the notes"));
    assert_eq!(steps.len(), 1);
    assert!(steps[0].starts_with("\"STEP_SENTINEL"));
    let (text, payload) = crate::compactor::restore_checkpoint(checkpoint);
    assert!(payload.is_some());
    assert_eq!(text, user_text(&agent.history[0]));
    assert_eq!(
        entries[1],
        turn_log::Logged::Turn {
            user: "read the notes".to_owned(),
            steps: Vec::new(),
            steering: Vec::new(),
            files: Vec::new(),
            end: r#"replied "done" replay=false"#.to_owned(),
        }
    );
}

#[tokio::test]
async fn a_turn_dropped_after_its_checkpoint_stays_counted_as_a_logged_turn() {
    let big_reply = format!("HISTORY_SENTINEL {}", "h".repeat(150_000));
    let provider = FakeProvider::new(vec![
        spoken_tool_reply("Reading first.", "call-1", r#"{"value":"first.txt"}"#),
        unmetered(text_reply(&big_reply)),
        unmetered(text_reply(
            "Turn 1\nIn between: Read the file.\nT1: echoed first.txt",
        )),
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::InvalidRequest, "BadRequest"),
        ),
    ]);
    let (agent, _) = windowed(&provider, 45_000, 64);
    let (log, entries) = turn_log::MemoryLog::shared();
    let mut agent = turn_log::logged(agent, log);
    run(&mut agent, "first").await;
    let (second, _) = run(&mut agent, "second").await;
    assert_eq!(second.outcome, TurnOutcome::Failed);
    let entries = entries.lock().unwrap().clone();
    assert_eq!(entries.len(), 3, "{entries:?}");
    assert!(matches!(
        &entries[1],
        turn_log::Logged::Compaction { user: Some(user), .. } if user == "second"
    ));
    assert!(agent.turn_starts.is_empty());
    assert_eq!(agent.ledger.records, [turn_ledger::TurnRecord::LogOnly]);
    assert_eq!(
        agent
            .ledger
            .logged_cut(crate::execution_memory::Cut::default()),
        HistoryCut {
            turns: 1,
            tool_steps: 0,
            ..HistoryCut::default()
        }
    );
}

#[tokio::test]
async fn a_checkpoint_that_cannot_be_saved_fails_the_turn_and_is_not_installed() {
    let big_step = format!("STEP_SENTINEL {}", "h".repeat(150_000));
    let provider = FakeProvider::new(vec![
        spoken_tool_reply(&big_step, "call-1", r#"{"value":"notes.md"}"#),
        unmetered(text_reply(
            "Turn in progress\nIn between: Read the notes.\nT1: echoed notes.md",
        )),
    ]);
    let (agent, _) = windowed(&provider, 45_000, 64);
    let mut agent = turn_log::logged(agent, Box::new(turn_log::MemoryLog::failing("SessionBusy")));
    let (report, events) = run(&mut agent, "read the notes").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(report.failure.unwrap().code(), "SessionBusy");
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::TurnCompaction {
            activity: CompactionActivity::Ended(CompactionEnd::Failed),
            ..
        }
    )));
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(user_text(&agent.history[0]), "read the notes");
    assert!(agent.compacted.is_none());
    assert_eq!(agent.history.len(), 3);
}
