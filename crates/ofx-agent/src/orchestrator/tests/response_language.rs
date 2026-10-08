use super::super::response_language::RESPONSE_LANGUAGE_FAILURE_NOTICE;
use super::turn_log::{Logged, MemoryLog, logged};
use super::*;
use crate::worker_runtime::{QueuedPrompt, WorkerRuntime};

const ENGLISH_PROMPT: &str = "The lockfile is broken again.";
const ENGLISH_REPLY: &str = "I will inspect the lockfile next.";
const CHINESE_REPLY: &str = "我会先检查锁文件和依赖清单。";
const RUSSIAN_REPLY: &str = "Сначала я проверю файл блокировки и манифест.";
const CORRECTION: &str = "The previous candidate used a different language";

fn chunked(chunks: &[&str]) -> Script {
    Script::Reply(
        chunks
            .iter()
            .map(|chunk| StreamEvent::TextDelta {
                text: (*chunk).to_owned(),
            })
            .collect(),
        completion(Some(&chunks.concat()), Vec::new(), FinishReason::Stop),
    )
}

fn streamed(events: &[UiEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::AssistantText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn system_notices(events: &[UiEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::SystemNotice { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn count_text(request: &SeenRequest, needle: &str) -> usize {
    let messages = request.messages.iter().map(|message| match message {
        ChatMessage::Assistant { content, .. } => content.as_deref().unwrap_or_default(),
        ChatMessage::User { content, .. }
        | ChatMessage::System { content }
        | ChatMessage::Tool { content, .. } => content.as_str(),
    });
    request
        .instructions
        .iter()
        .map(String::as_str)
        .chain(messages)
        .filter(|text| text.contains(needle))
        .count()
}

fn child_agent(provider: &Arc<FakeProvider>) -> Agent {
    let mut agent = new_agent(Arc::clone(provider), vec![echo_tool()]);
    agent.inherit_root_user_requests(Arc::new(RootUserRequests {
        current: ENGLISH_PROMPT.to_owned(),
        earlier: Vec::new(),
        compacted_turns: None,
    }));
    agent
}

#[tokio::test]
async fn only_root_turns_carry_the_response_language_authority() {
    let provider = FakeProvider::new(vec![text_reply("Done")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    run(&mut agent, "user prompt").await;
    assert_eq!(
        count_text(&provider.requests()[0], RESPONSE_LANGUAGE_CONTROL),
        1
    );

    let provider = FakeProvider::new(vec![text_reply("Done")]);
    let mut child = child_agent(&provider);
    run(&mut child, "user prompt").await;
    assert_eq!(
        count_text(&provider.requests()[0], RESPONSE_LANGUAGE_CONTROL),
        0
    );
}

#[tokio::test]
async fn a_reply_in_the_expected_script_streams_once_its_prefix_is_clear() {
    let provider = FakeProvider::new(vec![chunked(&[
        "I ",
        "will inspect ",
        "the lockfile next.",
    ])]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = run(&mut agent, ENGLISH_PROMPT).await;
    assert_eq!(report.final_text, ENGLISH_REPLY);
    assert_eq!(streamed(&events), ["I will inspect ", "the lockfile next."]);
}

#[tokio::test]
async fn matching_output_is_held_to_completion_after_conflicting_history() {
    let provider = FakeProvider::new(vec![
        text_reply(CHINESE_REPLY),
        chunked(&["I will inspect ", "the lockfile next."]),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    run(&mut agent, "Answer in Chinese.").await;
    let (report, events) = run(&mut agent, ENGLISH_PROMPT).await;
    assert_eq!(report.final_text, ENGLISH_REPLY);
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(streamed(&events), [ENGLISH_REPLY]);
}

#[tokio::test]
async fn one_clear_language_mismatch_is_retried_without_publishing_or_keeping_it() {
    let provider = FakeProvider::new(vec![
        text_reply(CHINESE_REPLY),
        text_reply(ENGLISH_REPLY),
        text_reply("Next."),
    ]);
    let (log, entries) = MemoryLog::shared();
    let mut agent = logged(new_agent(Arc::clone(&provider), Vec::new()), log);
    let (report, events) = run(&mut agent, ENGLISH_PROMPT).await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, ENGLISH_REPLY);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(count_text(&requests[0], CORRECTION), 0);
    assert_eq!(count_text(&requests[1], CORRECTION), 1);
    assert!(matches!(
        requests[1].messages.last(),
        Some(ChatMessage::User { content, .. }) if content.contains(CORRECTION)
    ));
    assert_eq!(count_text(&requests[1], ENGLISH_PROMPT), 1);
    assert_eq!(count_text(&requests[1], CHINESE_REPLY), 0);
    assert_eq!(streamed(&events), [ENGLISH_REPLY]);
    assert!(system_notices(&events).is_empty());
    assert_eq!(report.usage.output_tokens, Some(4));
    let entries = entries.lock().unwrap().clone();
    let [Logged::Turn { end, .. }] = entries.as_slice() else {
        panic!("expected one logged turn: {entries:?}");
    };
    assert_eq!(end, &format!("replied {ENGLISH_REPLY:?} replay=false"));
    run(&mut agent, "And now?").await;
    assert_eq!(count_text(&provider.requests()[2], CORRECTION), 0);
    assert_eq!(count_text(&provider.requests()[2], CHINESE_REPLY), 0);
}

#[tokio::test(start_paused = true)]
async fn a_restarted_reply_is_held_until_its_own_prefix_matches() {
    let provider = FakeProvider::new(vec![
        Script::Fail(
            vec![StreamEvent::TextDelta {
                text: "I will inspect ".to_owned(),
            }],
            failure(ProviderErrorKind::TransportInterrupted, "RequestFailed"),
        ),
        text_reply(CHINESE_REPLY),
        text_reply(ENGLISH_REPLY),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = run(&mut agent, ENGLISH_PROMPT).await;
    assert_eq!(report.final_text, ENGLISH_REPLY);
    assert_eq!(streamed(&events), ["I will inspect ", ENGLISH_REPLY]);
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn a_second_clear_mismatch_fails_the_turn_without_keeping_a_reply() {
    let provider = FakeProvider::new(vec![text_reply(CHINESE_REPLY), text_reply(RUSSIAN_REPLY)]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = run(&mut agent, ENGLISH_PROMPT).await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(report.failure, Some(TurnFailure::ResponseLanguageMismatch));
    assert_eq!(
        report.failure.as_ref().map(TurnFailure::code),
        Some("ResponseLanguageMismatch")
    );
    assert_eq!(provider.requests().len(), 2);
    assert!(streamed(&events).is_empty());
    assert_eq!(system_notices(&events), [RESPONSE_LANGUAGE_FAILURE_NOTICE]);
    assert!(agent.history.is_empty());
}

#[tokio::test]
async fn explicit_language_switches_are_left_alone() {
    let japanese = "次にロックファイルを確認します。";
    let provider = FakeProvider::new(vec![text_reply(japanese)]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = run(&mut agent, "Answer in Japanese and keep it short.").await;
    assert_eq!(report.final_text, japanese);
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(streamed(&events), [japanese]);
}

#[tokio::test]
async fn a_tool_bearing_response_keeps_its_calls_and_drops_its_prose() {
    let provider = FakeProvider::new(vec![
        Script::Reply(
            vec![StreamEvent::TextDelta {
                text: CHINESE_REPLY.to_owned(),
            }],
            Completion {
                provider_replay: Some(replay("prose and call")),
                ..completion(
                    Some(CHINESE_REPLY),
                    vec![echo_call("call-1", "{}")],
                    FinishReason::ToolCalls,
                )
            },
        ),
        text_reply("I inspected the lockfile."),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, ENGLISH_PROMPT).await;
    assert_eq!(report.final_text, "I inspected the lockfile.");
    assert_eq!(finished(&events), [("call-1", ToolResultStatus::Success)]);
    assert_eq!(streamed(&events), ["I inspected the lockfile."]);
    assert_eq!(
        provider.projections(),
        [("prose and call".to_owned(), false, true)]
    );
    let ChatMessage::Assistant {
        content,
        tool_calls,
        provider_replay,
    } = &provider.requests()[1].messages[1]
    else {
        panic!("expected the tool step");
    };
    assert_eq!(content, &None);
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(
        provider_replay
            .as_ref()
            .map(|replay| replay.parts_json.as_str()),
        Some("reasoning of prose and call")
    );
}

#[tokio::test]
async fn prompts_without_an_english_signal_set_no_expectation() {
    let provider = FakeProvider::new(vec![text_reply(CHINESE_REPLY)]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = run(&mut agent, "fix lockfile").await;
    assert_eq!(report.final_text, CHINESE_REPLY);
    assert_eq!(streamed(&events), [CHINESE_REPLY]);
}

#[tokio::test]
async fn children_answer_without_a_language_check() {
    let provider = FakeProvider::new(vec![text_reply(CHINESE_REPLY)]);
    let mut child = child_agent(&provider);
    let (report, _) = run(&mut child, ENGLISH_PROMPT).await;
    assert_eq!(report.final_text, CHINESE_REPLY);
    assert_eq!(provider.requests().len(), 1);
}

async fn interrupted_after(reply: &str) -> (Agent, TurnReport, Vec<UiEvent>) {
    let provider = FakeProvider::new(vec![Script::StreamThenWait(vec![StreamEvent::TextDelta {
        text: reply.to_owned(),
    }])]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    let mut record = |event| events.push(event);
    let (report, ()) = tokio::join!(
        agent.run_turn(ENGLISH_PROMPT, &mut record, &cancel),
        async {
            while provider.requests().is_empty() {
                tokio::task::yield_now().await;
            }
            tokio::task::yield_now().await;
            cancel.cancel();
        }
    );
    (agent, report, events)
}

fn kept_replies(agent: &Agent) -> Vec<&str> {
    agent
        .history
        .iter()
        .filter_map(|message| match message {
            ChatMessage::Assistant {
                content: Some(text),
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_interrupted_reply_keeps_only_text_in_the_expected_script() {
    let (agent, report, events) = interrupted_after(CHINESE_REPLY).await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert!(streamed(&events).is_empty());
    assert!(kept_replies(&agent).is_empty());

    let (agent, report, _) = interrupted_after("I will").await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(kept_replies(&agent), ["I will"]);
}

async fn steered_after_tool(steer: &str, replies: Vec<Script>) -> (Arc<FakeProvider>, TurnReport) {
    let mut scripts = vec![tool_reply(&[("call-1", "{}")])];
    scripts.extend(replies);
    let provider = FakeProvider::new(scripts);
    let worker = Arc::new(WorkerRuntime::default());
    let mut agent =
        new_agent(Arc::clone(&provider), vec![echo_tool()]).with_steering(Arc::clone(&worker));
    worker.admit(QueuedPrompt::new(0, ENGLISH_PROMPT.to_owned(), Vec::new()));
    let prompt = worker.take_next().expect("a queued prompt");
    let steer = steer.to_owned();
    let report = agent
        .run_turn(
            &prompt.text,
            &mut |event| {
                if matches!(event, UiEvent::ToolStarted { .. }) {
                    worker.admit(QueuedPrompt::new(1, steer.clone(), Vec::new()));
                }
            },
            &CancellationToken::new(),
        )
        .await;
    worker.finish_processing();
    (provider, report)
}

#[tokio::test]
async fn steering_that_asks_for_another_language_lifts_the_expectation() {
    let japanese = "次にロックファイルを確認します。";
    let (provider, report) = steered_after_tool(
        "Answer in Japanese and keep it short.",
        vec![text_reply(japanese)],
    )
    .await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, japanese);
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(count_text(&provider.requests()[1], CORRECTION), 0);
}

#[tokio::test]
async fn english_steering_keeps_the_expectation() {
    let (provider, report) = steered_after_tool(
        "Please also check the manifest.",
        vec![text_reply(CHINESE_REPLY), text_reply(ENGLISH_REPLY)],
    )
    .await;
    assert_eq!(report.final_text, ENGLISH_REPLY);
    assert_eq!(provider.requests().len(), 3);
    assert_eq!(count_text(&provider.requests()[2], CORRECTION), 1);
}

#[tokio::test]
async fn provisional_starts_do_not_flush_withheld_language_or_add_display_boundaries() {
    let provider = FakeProvider::new(vec![
        Script::Reply(
            vec![
                StreamEvent::TextDelta {
                    text: CHINESE_REPLY.to_owned(),
                },
                streamed_start("call-1", "echo"),
            ],
            completion(
                Some(CHINESE_REPLY),
                vec![echo_call("call-1", r#"{"text":"read"}"#)],
                FinishReason::ToolCalls,
            ),
        ),
        text_reply(ENGLISH_REPLY),
    ]);
    let mut agent = new_agent(
        provider,
        vec![stream_start_tool(
            ToolActivity::Read,
            Arc::new(AtomicUsize::new(0)),
        )],
    );
    let (report, events) = run(&mut agent, ENGLISH_PROMPT).await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let provisional = events
        .iter()
        .position(|event| matches!(event, UiEvent::ToolProvisional { .. }))
        .unwrap();
    assert!(!events[..provisional].iter().any(|event| matches!(
        event,
        UiEvent::AssistantText { .. } | UiEvent::AssistantBoundary { .. }
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, UiEvent::AssistantBoundary { .. }))
    );
    assert_eq!(streamed(&events), [ENGLISH_REPLY]);
    assert_eq!(finished(&events), [("call-1", ToolResultStatus::Success)]);
}

#[tokio::test(start_paused = true)]
async fn failed_withheld_prose_retries_after_a_provisional_start() {
    let provider = FakeProvider::new(vec![
        Script::Fail(
            vec![
                StreamEvent::TextDelta {
                    text: CHINESE_REPLY.to_owned(),
                },
                streamed_start("call-1", "echo"),
            ],
            failure(ProviderErrorKind::Unavailable, "Unavailable"),
        ),
        text_reply(ENGLISH_REPLY),
    ]);
    let mut agent = new_agent(
        Arc::clone(&provider),
        vec![stream_start_tool(
            ToolActivity::Read,
            Arc::new(AtomicUsize::new(0)),
        )],
    );
    let (report, events) = run(&mut agent, ENGLISH_PROMPT).await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = provider.requests();
    let [first, retried] = &requests[..] else {
        panic!("expected one retry: {requests:?}");
    };
    assert_eq!(retried.messages[..first.messages.len()], first.messages[..]);
    assert_eq!(retried.messages.len(), first.messages.len() + 1);
    assert_eq!(retried.tool_choice, ToolChoice::None);
    assert_eq!(streamed(&events), [ENGLISH_REPLY]);
    assert!(finished(&events).is_empty());
    assert!(
        events
            .iter()
            .any(|event| matches!(event, UiEvent::ToolProvisional { .. }))
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        UiEvent::AssistantBoundary { .. } | UiEvent::AssistantRestarted { .. }
    )));
}
