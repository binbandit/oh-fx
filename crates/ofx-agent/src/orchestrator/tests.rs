use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ofx_contract::{
    CallDescription, Concurrency, ModelRecoveryAction, PreparedCall, StreamSink, ToolActivity,
    ToolCallId, ToolEffect,
};

use super::*;

const SYSTEM_PROMPT: &str = "# Identity and context";
const TURN_CONTEXT: &str = "<fx-turn-context>\n</fx-turn-context>";

enum Script {
    Reply(Vec<StreamEvent>, Completion),
    Fail(Vec<StreamEvent>, ProviderError),
    WaitForCancel,
}

#[derive(Debug, Clone, PartialEq)]
struct SeenRequest {
    model: String,
    instructions: Vec<String>,
    messages: Vec<ChatMessage>,
    tools: Vec<ToolSpec>,
    max_output_tokens: Option<u32>,
}

struct FakeProvider {
    scripts: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<SeenRequest>>,
}

impl FakeProvider {
    fn new(scripts: Vec<Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl ModelProvider for FakeProvider {
    fn stream<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        sink: &'a mut dyn StreamSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        self.requests.lock().unwrap().push(SeenRequest {
            model: request.model.to_owned(),
            instructions: request
                .instructions
                .iter()
                .map(|text| (*text).to_owned())
                .collect(),
            messages: request.messages.to_vec(),
            tools: request.tools.to_vec(),
            max_output_tokens: request.max_output_tokens,
        });
        let script = self.scripts.lock().unwrap().pop_front();
        Box::pin(async move {
            match script {
                Some(Script::Reply(stream, completion)) => {
                    for event in stream {
                        sink.emit(event);
                    }
                    Ok(completion)
                }
                Some(Script::Fail(stream, error)) => {
                    for event in stream {
                        sink.emit(event);
                    }
                    Err(error)
                }
                Some(Script::WaitForCancel) => {
                    cancel.cancelled().await;
                    Err(ProviderError::cancelled())
                }
                None => Err(ProviderError::new(ProviderErrorKind::Protocol, "NoScript")),
            }
        })
    }
}

struct FixedContext;

impl RuntimeContext for FixedContext {
    fn runtime_context(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async { vec![TURN_CONTEXT.to_owned()] })
    }
}

struct ArgumentGate;

impl PermissionGate for ArgumentGate {
    fn admit(&self, call: &ToolCall) -> Admission {
        if call.arguments.contains("outside") {
            Admission::ApprovalRequired
        } else if call.arguments.contains("external") {
            Admission::Allowed(PathAccess::WorkspaceOrExternal)
        } else {
            Admission::Allowed(PathAccess::WorkspaceOnly)
        }
    }
}

struct EchoTool {
    spec: ToolSpec,
    cleaned_up: Arc<AtomicBool>,
    meeting: Arc<tokio::sync::Barrier>,
}

struct EchoCall {
    arguments: String,
    cleaned_up: Arc<AtomicBool>,
    meeting: Arc<tokio::sync::Barrier>,
}

impl Tool for EchoTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        if arguments.contains("invalid") {
            return Err(ToolOutput::failure("invalid arguments"));
        }
        assert!(!arguments.contains("prepare_panic"), "prepare panicked");
        Ok(Box::new(EchoCall {
            arguments: arguments.to_owned(),
            cleaned_up: Arc::clone(&self.cleaned_up),
            meeting: Arc::clone(&self.meeting),
        }))
    }
}

impl PreparedCall for EchoCall {
    fn describe(&self) -> CallDescription {
        assert!(
            !self.arguments.contains("describe_panic"),
            "describe panicked"
        );
        CallDescription {
            title: format!("Echoing {}", self.arguments),
            activity: ToolActivity::Read,
            effect: if self.arguments.contains("inert") {
                ToolEffect::None
            } else {
                ToolEffect::ReadOnly
            },
            concurrency: if self.arguments.contains("serial") {
                Concurrency::Serial
            } else {
                Concurrency::Parallel
            },
        }
    }

    fn execute(self: Box<Self>, context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            assert!(!self.arguments.contains("panic"), "echo panicked");
            if self.arguments.contains("meet") {
                self.meeting.wait().await;
            }
            if self.arguments.contains("hang") {
                std::future::pending::<()>().await;
            }
            if self.arguments.contains("wait") {
                context.cancellation.cancelled().await;
                tokio::task::yield_now().await;
                self.cleaned_up.store(true, Ordering::SeqCst);
                return ToolOutput::failure("stopped after cleanup");
            }
            if self.arguments.contains("access") {
                return ToolOutput::success(format!("{:?}", context.path_access));
            }
            if self.arguments.contains("fail") {
                ToolOutput::failure("echo failed")
            } else {
                ToolOutput::success(format!("echo {}", self.arguments))
            }
        })
    }
}

fn echo_tool() -> Arc<dyn Tool> {
    echo_tool_with(Arc::new(AtomicBool::new(false)))
}

fn echo_tool_with(cleaned_up: Arc<AtomicBool>) -> Arc<dyn Tool> {
    Arc::new(EchoTool {
        spec: ToolSpec {
            name: "echo".to_owned(),
            description: "Echo the arguments.".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        },
        cleaned_up,
        meeting: Arc::new(tokio::sync::Barrier::new(2)),
    })
}

fn completion(
    content: Option<&str>,
    tool_calls: Vec<ToolCall>,
    finish_reason: FinishReason,
) -> Completion {
    Completion {
        content: content.map(str::to_owned),
        tool_calls,
        finish_reason,
        usage: Usage {
            input_tokens: Some(10),
            output_tokens: Some(2),
        },
    }
}

fn text_reply(text: &str) -> Script {
    Script::Reply(
        vec![StreamEvent::TextDelta {
            text: text.to_owned(),
        }],
        completion(Some(text), Vec::new(), FinishReason::Stop),
    )
}

fn echo_call(id: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(id),
        name: "echo".to_owned(),
        arguments: arguments.to_owned(),
    }
}

fn tool_reply(calls: &[(&str, &str)]) -> Script {
    let calls = calls
        .iter()
        .map(|(id, arguments)| echo_call(id, arguments))
        .collect();
    Script::Reply(Vec::new(), completion(None, calls, FinishReason::ToolCalls))
}

fn failure(kind: ProviderErrorKind, code: &str) -> ProviderError {
    ProviderError::new(kind, code)
}

fn config() -> AgentConfig {
    AgentConfig {
        model: "test-model".to_owned(),
        system_prompt: SYSTEM_PROMPT.to_owned(),
        max_output_tokens: Some(64),
        step_limit: 0,
    }
}

fn new_agent(provider: Arc<FakeProvider>, tools: Vec<Arc<dyn Tool>>) -> Agent {
    Agent::new(
        provider,
        tools,
        Arc::new(FixedContext),
        Arc::new(ArgumentGate),
        config(),
    )
}

async fn run(agent: &mut Agent, prompt: &str) -> (TurnReport, Vec<UiEvent>) {
    let mut events = Vec::new();
    let report = agent
        .run_turn(
            prompt,
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await;
    (report, events)
}

fn assistant_text(events: &[UiEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::AssistantText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn recoveries(events: &[UiEvent]) -> Vec<RouteRecoveryStatus> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::Recovery { status, .. } => Some(status.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn final_answers_stream_raw_text_and_complete_the_turn() {
    let provider = FakeProvider::new(vec![text_reply("\n  Hello there")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = run(&mut agent, "hi").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "\n  Hello there");
    assert_eq!(report.usage.input_tokens, Some(10));
    assert_eq!(assistant_text(&events), "\n  Hello there");
    assert!(matches!(events.first(), Some(UiEvent::TurnStarted { .. })));
    assert!(matches!(
        events.last(),
        Some(UiEvent::TurnFinished {
            outcome: TurnOutcome::Completed,
            ..
        })
    ));
    let requests = provider.requests();
    assert_eq!(
        requests,
        [SeenRequest {
            model: "test-model".to_owned(),
            instructions: vec![
                SYSTEM_PROMPT.to_owned(),
                TURN_CONTEXT.to_owned(),
                RESPONSE_LANGUAGE_CONTROL.to_owned()
            ],
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            max_output_tokens: Some(64),
        }]
    );
}

#[tokio::test]
async fn an_empty_system_prompt_is_left_out_of_the_instructions() {
    let provider = FakeProvider::new(vec![text_reply("ok")]);
    let config = AgentConfig {
        system_prompt: String::new(),
        ..config()
    };
    let shared: Arc<FakeProvider> = Arc::clone(&provider);
    let mut agent = Agent::new(
        shared,
        Vec::new(),
        Arc::new(FixedContext),
        Arc::new(ArgumentGate),
        config,
    );
    run(&mut agent, "hi").await;
    assert_eq!(
        provider.requests()[0].instructions,
        [TURN_CONTEXT, RESPONSE_LANGUAGE_CONTROL]
    );
}

#[tokio::test]
async fn tool_calls_run_and_feed_results_back() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"text":"a"}"#)]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "echo please").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.usage.input_tokens, Some(20));
    let started = events
        .iter()
        .find_map(|event| match event {
            UiEvent::ToolStarted { description, .. } => Some(description.title.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(started, r#"Echoing {"text":"a"}"#);
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::ToolFinished {
            status: ToolResultStatus::Success,
            ..
        }
    )));
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools.len(), 1);
    let follow_up = &requests[1].messages;
    assert_eq!(follow_up.len(), 3);
    assert!(
        matches!(&follow_up[1], ChatMessage::Assistant { tool_calls, .. } if tool_calls.len() == 1)
    );
    assert_eq!(
        follow_up[2],
        ChatMessage::Tool {
            call_id: ToolCallId::new("call-1"),
            tool_name: "echo".to_owned(),
            content: r#"echo {"text":"a"}"#.to_owned(),
            status: ToolResultStatus::Success,
        }
    );
    assert_eq!(agent.history.len(), 4);
}

#[tokio::test]
async fn unknown_tools_and_rejected_arguments_are_reported_and_panics_become_failures() {
    let provider = FakeProvider::new(vec![
        Script::Reply(
            Vec::new(),
            completion(
                None,
                vec![ToolCall {
                    id: ToolCallId::new("call-0"),
                    name: "missing".to_owned(),
                    arguments: "{}".to_owned(),
                }],
                FinishReason::ToolCalls,
            ),
        ),
        tool_reply(&[
            ("call-1", r#"{"invalid":true}"#),
            ("call-2", r#"{"panic":true}"#),
        ]),
        text_reply("ok"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let rejected: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ToolRejected { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(rejected, ["call-0", "call-1"]);
    let started = events
        .iter()
        .filter(|event| matches!(event, UiEvent::ToolStarted { .. }))
        .count();
    assert_eq!(started, 1);
    let messages = &provider.requests()[2].messages;
    let results: Vec<&str> = messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::Tool { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results[0], "Unsupported tool: missing");
    assert_eq!(results[1], "invalid arguments");
    assert_eq!(
        results[2],
        r#"{"error":{"type":"tool_execution_failed","tool_name":"echo","message":"Tool execution panicked"}}"#
    );
}

#[tokio::test]
async fn identical_failures_escalate_within_a_turn() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"fail":1}"#)]),
        tool_reply(&[("call-2", r#"{"fail":1}"#)]),
        text_reply("giving up"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    run(&mut agent, "go").await;
    let messages = &provider.requests()[2].messages;
    let ChatMessage::Tool { content, .. } = messages.last().unwrap() else {
        panic!("expected a tool result");
    };
    assert_eq!(
        content,
        "echo failed\n\nThis exact call has already failed 2 times this turn with the same arguments. Do not retry it unchanged."
    );
}

#[tokio::test]
async fn step_limits_stop_the_loop_with_the_upstream_notice_and_keep_the_turn() {
    let provider = FakeProvider::new(vec![tool_reply(&[("call-1", "{}")]), text_reply("never")]);
    let mut agent = Agent::new(
        provider,
        vec![echo_tool()],
        Arc::new(FixedContext),
        Arc::new(ArgumentGate),
        AgentConfig {
            step_limit: 1,
            ..config()
        },
    );
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(report.failure, Some(TurnFailure::StepLimitReached));
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::Operational { text, .. } if *text == format!("{STEP_LIMIT_NOTICE}\n")
    )));
    assert_eq!(
        agent.history.last(),
        Some(&ChatMessage::Assistant {
            content: Some(STEP_LIMIT_NOTICE.to_owned()),
            tool_calls: Vec::new(),
        })
    );
}

#[tokio::test]
async fn empty_answers_become_done_and_silent_tool_steps_ask_for_a_summary() {
    let provider = FakeProvider::new(vec![text_reply("   ")]);
    let mut empty = new_agent(provider, Vec::new());
    let (report, events) = run(&mut empty, "go").await;
    assert_eq!(report.final_text, EMPTY_RESPONSE_TEXT);
    assert_eq!(assistant_text(&events), "   ");
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::Operational { text, .. } if text == EMPTY_RESPONSE_TEXT
    )));
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        tool_reply(&[("call-2", "{}")]),
        text_reply(""),
        text_reply("Summary."),
    ]);
    let mut silent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, _) = run(&mut silent, "go").await;
    assert_eq!(report.final_text, "Summary.");
    let requests = provider.requests();
    assert_eq!(
        requests[3].messages.last(),
        Some(&ChatMessage::user(SUMMARIZE_PROMPT))
    );
}

#[tokio::test]
async fn whitespace_tool_step_prose_is_not_silent() {
    let with_space = |id: &str| {
        Script::Reply(
            Vec::new(),
            completion(
                Some(" "),
                vec![echo_call(id, "{}")],
                FinishReason::ToolCalls,
            ),
        )
    };
    let provider = FakeProvider::new(vec![
        with_space("call-1"),
        with_space("call-2"),
        text_reply(""),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.final_text, EMPTY_RESPONSE_TEXT);
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn provider_failures_drop_an_empty_turn_but_keep_executed_tool_steps() {
    let error = failure(ProviderErrorKind::Unauthorized, "unauthorized");
    let provider = FakeProvider::new(vec![Script::Fail(Vec::new(), error.clone())]);
    let mut agent = new_agent(provider, Vec::new());
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(report.failure, Some(TurnFailure::Provider(error.clone())));
    assert!(agent.history.is_empty());
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        Script::Fail(Vec::new(), error.clone()),
    ]);
    let mut agent = new_agent(provider, vec![echo_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(agent.history.len(), 3);
    assert!(matches!(agent.history[2], ChatMessage::Tool { .. }));
    let partial = vec![StreamEvent::TextDelta {
        text: "Partial answer".to_owned(),
    }];
    let provider = FakeProvider::new(vec![Script::Fail(
        partial,
        failure(ProviderErrorKind::TransportInterrupted, "ReadFailed"),
    )]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(
        agent.history.last(),
        Some(&ChatMessage::Assistant {
            content: Some("Partial answer".to_owned()),
            tool_calls: Vec::new(),
        })
    );
}

#[tokio::test]
async fn cancellation_interrupts_the_turn_and_keeps_the_prompt() {
    let provider = FakeProvider::new(vec![Script::WaitForCancel]);
    let mut agent = new_agent(provider, Vec::new());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move { trigger.cancel() });
    let report = agent.run_turn("go", &mut |_| {}, &cancel).await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(agent.history, [ChatMessage::user("go")]);
}

async fn run_cancelled_at(agent: &mut Agent, cancel_at: &str) -> (TurnReport, Vec<UiEvent>) {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let cancel_at = cancel_at.to_owned();
    let mut events = Vec::new();
    let report = agent
        .run_turn(
            "go",
            &mut |event| {
                if matches!(&event, UiEvent::ToolStarted { call_id, .. } if call_id.as_str() == cancel_at)
                {
                    trigger.cancel();
                }
                events.push(event);
            },
            &cancel,
        )
        .await;
    (report, events)
}

fn finished(events: &[UiEvent]) -> Vec<(&str, ToolResultStatus)> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ToolFinished {
                call_id, status, ..
            } => Some((call_id.as_str(), *status)),
            _ => None,
        })
        .collect()
}

fn dispatch_order(events: &[UiEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ToolStarted { call_id, .. } => Some(format!("start {}", call_id.as_str())),
            UiEvent::ToolFinished { call_id, .. } => Some(format!("finish {}", call_id.as_str())),
            _ => None,
        })
        .collect()
}

fn tool_message(id: &str, content: &str, status: ToolResultStatus) -> ChatMessage {
    ChatMessage::Tool {
        call_id: ToolCallId::new(id),
        tool_name: "echo".to_owned(),
        content: content.to_owned(),
        status,
    }
}

#[tokio::test]
async fn cancelled_tools_that_finish_within_the_grace_period_keep_their_results() {
    let cleaned_up = Arc::new(AtomicBool::new(false));
    let provider = FakeProvider::new(vec![tool_reply(&[
        ("call-1", r#"{"text":"before"}"#),
        ("call-2", r#"{"wait":true}"#),
    ])]);
    let mut agent = new_agent(provider, vec![echo_tool_with(Arc::clone(&cleaned_up))]);
    let (report, events) = run_cancelled_at(&mut agent, "call-2").await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert!(cleaned_up.load(Ordering::SeqCst));
    assert_eq!(
        finished(&events),
        [
            ("call-1", ToolResultStatus::Success),
            ("call-2", ToolResultStatus::Failure)
        ]
    );
    assert_eq!(
        agent.history,
        [
            ChatMessage::user("go"),
            ChatMessage::Assistant {
                content: None,
                tool_calls: vec![
                    echo_call("call-1", r#"{"text":"before"}"#),
                    echo_call("call-2", r#"{"wait":true}"#),
                ],
            },
            tool_message(
                "call-1",
                r#"echo {"text":"before"}"#,
                ToolResultStatus::Success
            ),
            tool_message("call-2", "stopped after cleanup", ToolResultStatus::Failure),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn cancelled_tools_that_outlive_the_grace_period_are_aborted_and_dropped_from_history() {
    let provider = FakeProvider::new(vec![tool_reply(&[
        ("call-1", r#"{"text":"before"}"#),
        ("call-2", r#"{"hang":true}"#),
    ])]);
    let mut agent = new_agent(provider, vec![echo_tool()]);
    let (report, events) = run_cancelled_at(&mut agent, "call-2").await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(
        finished(&events),
        [
            ("call-1", ToolResultStatus::Success),
            ("call-2", ToolResultStatus::Failure)
        ]
    );
    assert_eq!(
        agent.history,
        [
            ChatMessage::user("go"),
            ChatMessage::Assistant {
                content: None,
                tool_calls: vec![echo_call("call-1", r#"{"text":"before"}"#)],
            },
            tool_message(
                "call-1",
                r#"echo {"text":"before"}"#,
                ToolResultStatus::Success
            ),
        ]
    );
}

#[tokio::test]
async fn cancelling_while_a_parallel_group_starts_never_starts_the_rest() {
    let provider = FakeProvider::new(vec![tool_reply(&[
        ("call-1", r#"{"wait":true}"#),
        ("call-2", r#"{"text":"later"}"#),
        ("call-3", r#"{"invalid":true}"#),
    ])]);
    let mut agent = new_agent(provider, vec![echo_tool()]);
    let (report, events) = run_cancelled_at(&mut agent, "call-1").await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(dispatch_order(&events), ["start call-1", "finish call-1"]);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, UiEvent::ToolRejected { .. }))
    );
    assert_eq!(
        agent.history,
        [
            ChatMessage::user("go"),
            ChatMessage::Assistant {
                content: None,
                tool_calls: vec![echo_call("call-1", r#"{"wait":true}"#)],
            },
            tool_message("call-1", "stopped after cleanup", ToolResultStatus::Failure),
        ]
    );
}

#[tokio::test]
async fn parallel_calls_overlap_and_report_results_in_call_order() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"meet":1}"#), ("call-2", r#"{"meet":2}"#)]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = tokio::time::timeout(Duration::from_secs(10), run(&mut agent, "go"))
        .await
        .expect("parallel calls overlap");
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        dispatch_order(&events),
        [
            "start call-1",
            "start call-2",
            "finish call-1",
            "finish call-2"
        ]
    );
    assert_eq!(
        provider.requests()[1].messages[2..],
        [
            tool_message("call-1", r#"echo {"meet":1}"#, ToolResultStatus::Success),
            tool_message("call-2", r#"echo {"meet":2}"#, ToolResultStatus::Success),
        ]
    );
}

#[tokio::test]
async fn admitted_calls_run_with_the_path_access_their_admission_grants() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"access":"workspace"}"#),
            ("call-2", r#"{"access":"external"}"#),
            ("call-3", r#"{"access":"outside","inert":true}"#),
        ]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        provider.requests()[1].messages[2..],
        [
            tool_message("call-1", "WorkspaceOnly", ToolResultStatus::Success),
            tool_message("call-2", "WorkspaceOrExternal", ToolResultStatus::Success),
            tool_message("call-3", "WorkspaceOnly", ToolResultStatus::Success),
        ]
    );
}

#[tokio::test]
async fn a_call_that_needs_approval_fails_the_turn_after_earlier_calls_settle() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"text":"before"}"#),
            ("call-2", r#"{"path":"outside"}"#),
            ("call-3", r#"{"text":"after"}"#),
        ]),
        text_reply("never"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    let failure = report.failure.unwrap();
    assert_eq!(failure.code(), "NonInteractivePermissionRequired");
    assert_eq!(
        failure,
        TurnFailure::PermissionRequired(BlockedCall {
            tool_name: "echo".to_owned(),
            title: r#"Echoing {"path":"outside"}"#.to_owned(),
        })
    );
    assert_eq!(
        dispatch_order(&events),
        ["start call-1", "start call-2", "finish call-1"]
    );
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(
        agent.history,
        [
            ChatMessage::user("go"),
            ChatMessage::Assistant {
                content: None,
                tool_calls: vec![echo_call("call-1", r#"{"text":"before"}"#)],
            },
            tool_message(
                "call-1",
                r#"echo {"text":"before"}"#,
                ToolResultStatus::Success
            ),
        ]
    );
}

#[tokio::test]
async fn a_lone_call_that_needs_approval_leaves_no_history() {
    let provider = FakeProvider::new(vec![tool_reply(&[("call-1", r#"{"path":"outside"}"#)])]);
    let mut agent = new_agent(provider, vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(dispatch_order(&events), ["start call-1"]);
    assert!(agent.history.is_empty());
}

#[tokio::test]
async fn serial_calls_never_overlap_their_neighbours() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"text":"a"}"#),
            ("call-2", r#"{"serial":true}"#),
            ("call-3", r#"{"text":"b"}"#),
            ("call-4", r#"{"text":"c"}"#),
        ]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(provider, vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        dispatch_order(&events),
        [
            "start call-1",
            "finish call-1",
            "start call-2",
            "finish call-2",
            "start call-3",
            "start call-4",
            "finish call-3",
            "finish call-4"
        ]
    );
}

#[tokio::test]
async fn panics_while_preparing_or_describing_a_call_become_rejected_failures() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"prepare_panic":true}"#),
            ("call-2", r#"{"describe_panic":true}"#),
            ("call-3", r#"{"text":"after"}"#),
        ]),
        text_reply("ok"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let rejected: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ToolRejected { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(rejected, ["call-1", "call-2"]);
    let panicked = r#"{"error":{"type":"tool_execution_failed","tool_name":"echo","message":"Tool execution panicked"}}"#;
    assert_eq!(
        provider.requests()[1].messages[2..],
        [
            tool_message("call-1", panicked, ToolResultStatus::Failure),
            tool_message("call-2", panicked, ToolResultStatus::Failure),
            tool_message(
                "call-3",
                r#"echo {"text":"after"}"#,
                ToolResultStatus::Success
            ),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn retryable_failures_retry_with_upstream_pacing_and_report_recovery() {
    let mut rate_limited = failure(ProviderErrorKind::RateLimited, "rate_limited");
    rate_limited.retry_after = Some(Duration::from_secs(2));
    rate_limited.diagnostic = Some("HTTP 429 · slow".to_owned());
    let mut unavailable = failure(ProviderErrorKind::ServerError, "server_error");
    unavailable.diagnostic = Some("HTTP 500 · boom".to_owned());
    let provider = FakeProvider::new(vec![
        Script::Fail(Vec::new(), unavailable),
        Script::Fail(Vec::new(), rate_limited),
        text_reply("ok"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let started = Instant::now();
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(provider.requests().len(), 3);
    assert_eq!(started.elapsed(), Duration::from_millis(2_250));
    let labels: Vec<String> = recoveries(&events)
        .iter()
        .map(RouteRecoveryStatus::label)
        .collect();
    assert_eq!(
        labels,
        [
            "⚠ Provider unavailable · HTTP 500 · boom · retrying request",
            "⚠ Provider unavailable · HTTP 500 · boom · retrying request",
            "⚠ Rate limited · HTTP 429 · slow · retrying request in 2s",
            "⚠ Rate limited · HTTP 429 · slow · retrying request in 2s",
            "✓ recovered · succeeded on attempt 3",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn retries_stop_after_the_attempt_budget_and_skip_permanent_failures() {
    let scripts = (0..12)
        .map(|_| {
            Script::Fail(
                Vec::new(),
                failure(ProviderErrorKind::ConnectivityLost, "ConnectionFailed"),
            )
        })
        .collect();
    let provider = FakeProvider::new(scripts);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.failure.unwrap().code(), "ConnectionFailed");
    assert_eq!(provider.requests().len(), DEFAULT_MAX_PROVIDER_ATTEMPTS);
    let statuses = recoveries(&events);
    assert!(
        statuses
            .iter()
            .all(|status| status.action == Some(ModelRecoveryAction::WaitingForConnectivity))
    );
    assert_eq!(
        statuses[0].label(),
        "⚠ Connection lost · waiting for connection · 1s"
    );
    let provider = FakeProvider::new(vec![Script::Fail(
        Vec::new(),
        failure(ProviderErrorKind::InvalidRequest, "invalid_request"),
    )]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(provider.requests().len(), 1);
    assert!(recoveries(&events).is_empty());
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_retry_wait_interrupts_the_turn() {
    let mut limited = failure(ProviderErrorKind::RateLimited, "rate_limited");
    limited.retry_after = Some(Duration::from_secs(30));
    let provider = FakeProvider::new(vec![Script::Fail(Vec::new(), limited), text_reply("late")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let report = agent
        .run_turn(
            "go",
            &mut |event| {
                if matches!(event, UiEvent::Recovery { .. }) {
                    trigger.cancel();
                }
            },
            &cancel,
        )
        .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn later_turns_replay_history() {
    let provider = FakeProvider::new(vec![text_reply("first"), text_reply("second")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    run(&mut agent, "one").await;
    run(&mut agent, "two").await;
    let requests = provider.requests();
    assert_eq!(
        requests[1].messages,
        vec![
            ChatMessage::user("one"),
            ChatMessage::Assistant {
                content: Some("first".to_owned()),
                tool_calls: Vec::new(),
            },
            ChatMessage::user("two"),
        ]
    );
}

#[tokio::test]
async fn invalid_completions_fail_the_turn() {
    let provider = FakeProvider::new(vec![Script::Reply(
        Vec::new(),
        completion(None, Vec::new(), FinishReason::ToolCalls),
    )]);
    let mut agent = new_agent(provider, Vec::new());
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.failure, Some(TurnFailure::InvalidCompletion));
    assert_eq!(report.failure.unwrap().code(), "ModelError");
}
