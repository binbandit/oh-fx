use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use std::path::PathBuf;

use ofx_contract::{
    ActionLabel, ApplicableTarget, AutoCompactPercent, CallDescription, CommandProfile,
    CommandRequest, Concurrency, FileMutation, FileMutationState, ModelRecoveryAction,
    PreparedCall, ProviderReplay, ReasoningEffort, ReplaySource, RootUserRequests, StreamSink,
    SubagentStatus, ToolActivity, ToolCallId, ToolChoice, ToolEffect,
};

use super::*;

const SYSTEM_PROMPT: &str = "# Identity and context";
const TURN_CONTEXT: &str = "<fx-turn-context>\n</fx-turn-context>";
const PANICKED: &str = r#"{"error":{"type":"tool_execution_failed","tool_name":"echo","message":"Tool execution panicked"}}"#;

enum Script {
    Reply(Vec<StreamEvent>, Completion),
    Fail(Vec<StreamEvent>, ProviderError),
    Refuse(ProviderError),
    WaitForCancel,
    StreamThenWait(Vec<StreamEvent>),
}

#[derive(Debug, Clone, PartialEq)]
struct SeenRequest {
    model: String,
    instructions: Vec<String>,
    messages: Vec<ChatMessage>,
    tools: Vec<ToolSpec>,
    tool_choice: ToolChoice,
    max_output_tokens: Option<u32>,
    reasoning_effort: Option<String>,
    fast: bool,
}

struct FakeProvider {
    scripts: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<SeenRequest>>,
    projections: Mutex<Vec<(String, bool, bool)>>,
    bodies: Mutex<Vec<String>>,
    sessions: Mutex<Vec<Option<String>>>,
}

impl FakeProvider {
    fn new(scripts: Vec<Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
            projections: Mutex::new(Vec::new()),
            bodies: Mutex::new(Vec::new()),
            sessions: Mutex::new(Vec::new()),
        })
    }

    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn projections(&self) -> Vec<(String, bool, bool)> {
        self.projections.lock().unwrap().clone()
    }

    fn sessions(&self) -> Vec<Option<String>> {
        self.sessions.lock().unwrap().clone()
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
            tool_choice: request.tool_choice,
            max_output_tokens: request.max_output_tokens,
            reasoning_effort: request.provider_options.reasoning_effort.map(str::to_owned),
            fast: request.provider_options.fast,
        });
        self.sessions
            .lock()
            .unwrap()
            .push(request.session_id.map(str::to_owned));
        let script = self.scripts.lock().unwrap().pop_front();
        Box::pin(async move {
            if !matches!(script, Some(Script::Refuse(_))) {
                sink.emit(StreamEvent::Admitted);
            }
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
                Some(Script::Refuse(error)) => Err(error),
                Some(Script::WaitForCancel) => {
                    cancel.cancelled().await;
                    Err(ProviderError::cancelled())
                }
                Some(Script::StreamThenWait(stream)) => {
                    for event in stream {
                        sink.emit(event);
                    }
                    cancel.cancelled().await;
                    Err(ProviderError::cancelled())
                }
                None => Err(ProviderError::new(ProviderErrorKind::Protocol, "NoScript")),
            }
        })
    }

    fn request_body(&self, request: &ModelRequest<'_>) -> Option<String> {
        Some(format!(
            "{:?} {:?} {:?}",
            request.instructions, request.messages, request.tools
        ))
    }

    fn stream_body<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        body: String,
        sink: &'a mut dyn StreamSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        self.bodies.lock().unwrap().push(body);
        self.stream(request, sink, cancel)
    }

    fn project_replay(
        &self,
        replay: &ProviderReplay,
        text: bool,
        reasoning: bool,
    ) -> Result<Option<ProviderReplay>, ProviderError> {
        self.projections
            .lock()
            .unwrap()
            .push((replay.parts_json.clone(), text, reasoning));
        if replay.parts_json == "invalid" {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "InvalidProviderState",
            ));
        }
        Ok(Some(ProviderReplay {
            parts_json: format!("reasoning of {}", replay.parts_json),
            ..replay.clone()
        }))
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

    fn admit_file_mutation(&self, mutation: &FileMutation) -> Admission {
        match mutation.state {
            FileMutationState::Unread => Admission::ReviewRequired,
            FileMutationState::Changes => Admission::ApprovalRequired,
            FileMutationState::Creates | FileMutationState::Unchanged => {
                Admission::Allowed(PathAccess::WorkspaceOrExternal)
            }
        }
    }

    fn admit_command(&self, request: &CommandRequest) -> Admission {
        match request {
            CommandRequest::Run { command, .. } if command == "git status" => {
                Admission::Allowed(PathAccess::WorkspaceOrExternal)
            }
            CommandRequest::Stop => Admission::ApprovalRequired,
            _ => Admission::ReviewRequired,
        }
    }

    fn applicable_target(&self, _call: &ToolCall) -> Option<ApplicableTarget> {
        None
    }

    fn forget_approvals(&self) {}
}

struct ReadOnlyGate;

impl PermissionGate for ReadOnlyGate {
    fn admit(&self, _call: &ToolCall) -> Admission {
        Admission::Allowed(PathAccess::WorkspaceOrExternal)
    }

    fn applicable_target(&self, _call: &ToolCall) -> Option<ApplicableTarget> {
        None
    }

    fn forget_approvals(&self) {}
}

#[derive(Default)]
struct RecordingGate {
    admitted: Mutex<Vec<String>>,
}

impl PermissionGate for RecordingGate {
    fn admit(&self, call: &ToolCall) -> Admission {
        self.admitted.lock().unwrap().push(call.name.clone());
        Admission::Allowed(PathAccess::WorkspaceOnly)
    }

    fn applicable_target(&self, _call: &ToolCall) -> Option<ApplicableTarget> {
        None
    }

    fn forget_approvals(&self) {}
}

struct EchoTool {
    spec: ToolSpec,
    cleaned_up: Arc<AtomicBool>,
    meeting: Arc<tokio::sync::Barrier>,
}

struct EchoCall {
    arguments: String,
    mutation: Option<FileMutation>,
    command: Option<CommandRequest>,
    refusal: Option<ToolOutput>,
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
        let mutation = [
            ("creates", FileMutationState::Creates),
            ("changes", FileMutationState::Changes),
            ("unread", FileMutationState::Unread),
        ]
        .into_iter()
        .find(|(word, _)| arguments.contains(word))
        .map(|(_, state)| FileMutation {
            target: PathBuf::from("/workspace/note.txt"),
            state,
        });
        let command = [("run_git", "git status"), ("run_rm", "rm -rf .")]
            .into_iter()
            .find(|(word, _)| arguments.contains(word))
            .map(|(_, command)| CommandRequest::Run {
                command: command.to_owned(),
                cwd: PathBuf::from("/workspace"),
                profile: CommandProfile::User,
                shell: None,
                terminal: false,
                reload: false,
            })
            .or_else(|| arguments.contains("stop").then_some(CommandRequest::Stop));
        Ok(Box::new(EchoCall {
            arguments: arguments.to_owned(),
            mutation,
            command,
            refusal: arguments.contains("refused").then(|| {
                ToolOutput::failure("refused arguments").with_context_notices(
                    arguments
                        .contains("noticed")
                        .then(|| "refusal notice".to_owned()),
                )
            }),
            cleaned_up: Arc::clone(&self.cleaned_up),
            meeting: Arc::clone(&self.meeting),
        }))
    }

    fn history_arguments(&self, arguments: &str) -> Option<String> {
        assert!(
            !arguments.contains("history_panic"),
            "history arguments panicked"
        );
        arguments
            .contains("in_history")
            .then(|| format!("history {arguments}"))
    }
}

impl PreparedCall for EchoCall {
    fn untargeted_label(&self) -> Option<ActionLabel> {
        assert!(
            !self.arguments.contains("untargeted_panic"),
            "untargeted title panicked"
        );
        Some(ActionLabel {
            active: "Echoing",
            completed: "Echoed",
            target: "file".to_owned(),
        })
    }

    fn file_mutation(&self) -> Option<&FileMutation> {
        assert!(
            !self.arguments.contains("mutation_panic"),
            "file mutation panicked"
        );
        self.mutation.as_ref()
    }

    fn file_change(&self) -> Option<FileChange<'_>> {
        reviews::previewed_change(&self.arguments)
    }

    fn command_request(&self) -> Option<&CommandRequest> {
        assert!(
            !self.arguments.contains("command_panic"),
            "command request panicked"
        );
        self.command.as_ref()
    }

    fn mcp_tool(&self) -> bool {
        self.arguments.contains("mcp_call")
    }

    fn review_schema(&self) -> Option<String> {
        self.arguments
            .contains("mcp_schema")
            .then(|| format!("schema {}", self.arguments))
    }

    fn refusal(&self) -> Option<&ToolOutput> {
        assert!(
            !self.arguments.contains("refusal_panic"),
            "refusal panicked"
        );
        self.refusal.as_ref()
    }

    fn describe(&self) -> CallDescription {
        assert!(
            !self.arguments.contains("describe_panic"),
            "describe panicked"
        );
        if self.arguments.contains("describe_bomb") {
            panic::panic_any(Bomb);
        }
        CallDescription {
            title: format!("Echoing {}", self.arguments),
            label: None,
            activity: if self.arguments.contains("delegate") {
                ToolActivity::Subagent
            } else {
                ToolActivity::Read
            },
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
        assert!(
            !self.arguments.contains("execute_panic"),
            "execute panicked"
        );
        Box::pin(async move {
            assert!(!self.arguments.contains(r#""panic""#), "echo panicked");
            if self.arguments.contains("execute_bomb") {
                panic::panic_any(Bomb);
            }
            if self.arguments.contains("meet") {
                self.meeting.wait().await;
            }
            if self.arguments.contains(r#""hang""#) {
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
            if self.arguments.contains("intent") {
                return ToolOutput::success(format!("{:?}", context.root_user_requests));
            }
            if self.arguments.contains("status") {
                let Some(sink) = &context.subagent_status else {
                    return ToolOutput::success("no status sink");
                };
                sink.publish(SubagentStatus {
                    model: "child-model".to_owned(),
                    effort: ReasoningEffort::Named("high".to_owned()),
                });
                return ToolOutput::success("status published");
            }
            if self.arguments.contains("noticed") {
                return ToolOutput::success(format!("echo {}", self.arguments))
                    .with_context_notices(["echo notice".to_owned()]);
            }
            if self.arguments.contains("fail") {
                ToolOutput::failure("echo failed")
            } else {
                ToolOutput::success(format!("echo {}", self.arguments))
            }
        })
    }
}

impl Drop for EchoCall {
    fn drop(&mut self) {
        if self.arguments.contains("drop_panic") {
            self.cleaned_up.store(true, Ordering::SeqCst);
            panic!("drop panicked");
        }
    }
}

struct Bomb;

impl Drop for Bomb {
    fn drop(&mut self) {
        panic!("panic payload panicked while dropped");
    }
}

struct SpecReadOnce {
    inner: Arc<dyn Tool>,
    reads: Arc<AtomicUsize>,
}

impl Tool for SpecReadOnce {
    fn spec(&self) -> &ToolSpec {
        assert_eq!(
            self.reads.fetch_add(1, Ordering::SeqCst),
            0,
            "spec panicked"
        );
        self.inner.spec()
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        self.inner.prepare(arguments)
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
            input_schema: r#"{"type":"object"}"#.into(),
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
        provider_replay: None,
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
    ToolCall::new(id, "echo", arguments)
}

fn tool_reply(calls: &[(&str, &str)]) -> Script {
    let calls = calls
        .iter()
        .map(|(id, arguments)| echo_call(id, arguments))
        .collect();
    Script::Reply(Vec::new(), completion(None, calls, FinishReason::ToolCalls))
}

fn replay(parts: &str) -> ProviderReplay {
    ProviderReplay {
        source: ReplaySource {
            provider: "fake".to_owned(),
            model: "test-model".to_owned(),
        },
        parts_json: parts.to_owned(),
    }
}

fn with_replay(script: Script, parts: &str) -> Script {
    match script {
        Script::Reply(stream, mut completion) => {
            completion.provider_replay = Some(replay(parts));
            Script::Reply(stream, completion)
        }
        other => other,
    }
}

fn replays(messages: &[ChatMessage]) -> Vec<Option<&str>> {
    messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::Assistant {
                provider_replay, ..
            } => Some(
                provider_replay
                    .as_ref()
                    .map(|replay| replay.parts_json.as_str()),
            ),
            _ => None,
        })
        .collect()
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
        reasoning_effort: None,
        fast_mode: false,
        auto_compact_percent: AutoCompactPercent::new(80).unwrap(),
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
            tool_choice: ToolChoice::Auto,
            max_output_tokens: Some(64),
            reasoning_effort: None,
            fast: false,
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

struct ProviderTool {
    spec: ToolSpec,
}

impl Tool for ProviderTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn provider_executed(&self) -> bool {
        true
    }

    fn prepare(&self, _arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        Err(ToolOutput::failure("search is unavailable here"))
    }
}

fn provider_tool(name: &str, description: &str) -> Arc<dyn Tool> {
    Arc::new(ProviderTool {
        spec: ToolSpec {
            name: name.to_owned(),
            description: description.to_owned(),
            input_schema: r#"{"type":"object"}"#.into(),
        },
    })
}

#[tokio::test]
async fn provider_executed_tools_become_guidance_instead_of_functions() {
    let search = ToolCall::new("call-1", "search", r#"{"query":"news"}"#);
    let provider = FakeProvider::new(vec![
        Script::Reply(
            Vec::new(),
            completion(None, vec![search], FinishReason::ToolCalls),
        ),
        text_reply("done"),
    ]);
    let tools = vec![
        provider_tool("search", "Search the web."),
        echo_tool(),
        provider_tool("images", "Find images."),
    ];
    let mut agent = new_agent(Arc::clone(&provider), tools);
    let (report, _) = run(&mut agent, "look it up").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = provider.requests();
    assert_eq!(
        requests[0].instructions,
        [
            SYSTEM_PROMPT,
            "Search the web.\n\nFind images.",
            TURN_CONTEXT,
            RESPONSE_LANGUAGE_CONTROL
        ]
    );
    let offered: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(offered, ["echo"]);
    assert_eq!(
        requests[1].messages.last(),
        Some(&ChatMessage::Tool {
            call_id: ToolCallId::new("call-1"),
            tool_name: "search".to_owned(),
            content: "search is unavailable here".to_owned(),
            status: ToolResultStatus::Failure,
        })
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
                vec![ToolCall::new("call-0", "missing", "{}")],
                FinishReason::ToolCalls,
            ),
        ),
        tool_reply(&[
            ("call-1", r#"{"invalid":true}"#),
            ("call-2", r#"{"panic":true}"#),
            ("call-3", r#"{"refused":true}"#),
        ]),
        text_reply("ok"),
    ]);
    let gate = Arc::new(RecordingGate::default());
    let mut agent = Agent::new(
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        vec![echo_tool()],
        Arc::new(FixedContext),
        Arc::clone(&gate) as Arc<dyn PermissionGate>,
        config(),
    );
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        rejections(&events),
        [
            ("call-0", "missing", "{}", ToolRejection::Unsupported, None),
            (
                "call-1",
                "echo",
                r#"{"invalid":true}"#,
                ToolRejection::Invalid,
                None
            ),
            (
                "call-3",
                "echo",
                r#"{"refused":true}"#,
                ToolRejection::Invalid,
                Some(r#"Echoing {"refused":true}"#)
            ),
        ]
    );
    assert_eq!(*gate.admitted.lock().unwrap(), ["echo"]);
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
    assert_eq!(results[3], "refused arguments");
}

#[tokio::test]
async fn modern_mixed_batch_materializes_unsupported_terminal_before_admission() {
    let provider = FakeProvider::new(vec![
        Script::Reply(
            Vec::new(),
            completion(
                None,
                vec![
                    echo_call("candidate_read", r#"{"text":"input"}"#),
                    ToolCall::new("terminal_unsupported", "missing_tool", "{}"),
                ],
                FinishReason::ToolCalls,
            ),
        ),
        text_reply("Final"),
    ]);
    let gate = Arc::new(RecordingGate::default());
    let mut agent = Agent::new(
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        vec![echo_tool()],
        Arc::new(FixedContext),
        Arc::clone(&gate) as Arc<dyn PermissionGate>,
        config(),
    );
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(*gate.admitted.lock().unwrap(), ["echo"]);
    let lifecycle: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ToolStarted { call_id, .. } => Some(format!("start {}", call_id.as_str())),
            UiEvent::ToolFinished { call_id, .. } => Some(format!("finish {}", call_id.as_str())),
            UiEvent::ToolRejected { call_id, .. } => Some(format!("reject {}", call_id.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        lifecycle,
        [
            "start candidate_read",
            "finish candidate_read",
            "reject terminal_unsupported"
        ]
    );
    assert_eq!(
        rejections(&events),
        [(
            "terminal_unsupported",
            "missing_tool",
            "{}",
            ToolRejection::Unsupported,
            None
        )]
    );
    assert_eq!(
        provider.requests()[1].messages[2..],
        [
            tool_message(
                "candidate_read",
                r#"echo {"text":"input"}"#,
                ToolResultStatus::Success
            ),
            ChatMessage::Tool {
                call_id: ToolCallId::new("terminal_unsupported"),
                tool_name: "missing_tool".to_owned(),
                content: "Unsupported tool: missing_tool".to_owned(),
                status: ToolResultStatus::Failure,
            },
        ]
    );
}

#[tokio::test]
async fn calls_enter_the_history_in_their_tools_history_form_and_run_as_sent() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"in_history":true}"#),
            ("call-2", r#"{"in_history":true,"history_panic":true}"#),
            ("call-3", r#"{"text":"plain"}"#),
        ]),
        text_reply("ok"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let finished: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ToolFinished { arguments, .. } => Some(arguments.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        [
            r#"{"in_history":true}"#,
            r#"{"in_history":true,"history_panic":true}"#,
            r#"{"text":"plain"}"#
        ]
    );
    let messages = &provider.requests()[1].messages;
    assert_eq!(
        messages[1],
        ChatMessage::Assistant {
            content: None,
            tool_calls: vec![
                echo_call("call-1", r#"history {"in_history":true}"#),
                echo_call("call-2", r#"{"in_history":true,"history_panic":true}"#),
                echo_call("call-3", r#"{"text":"plain"}"#),
            ],
            provider_replay: None,
        }
    );
    assert_eq!(
        messages[2],
        tool_message(
            "call-1",
            r#"echo {"in_history":true}"#,
            ToolResultStatus::Success
        )
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
            provider_replay: None,
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
            provider_replay: None,
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

#[tokio::test]
async fn the_last_completed_reply_outlives_failed_and_interrupted_turns_until_a_clear() {
    let provider = FakeProvider::new(vec![
        text_reply("first answer"),
        Script::Fail(
            vec![StreamEvent::TextDelta {
                text: "partial".to_owned(),
            }],
            failure(ProviderErrorKind::InvalidRequest, "BadRequest"),
        ),
        text_reply("second answer"),
    ]);
    let mut agent = new_agent(provider, Vec::new());
    assert_eq!(agent.last_assistant_reply().as_deref(), None);
    run(&mut agent, "one").await;
    assert_eq!(
        agent.last_assistant_reply().as_deref(),
        Some("first answer")
    );
    let (failed, _) = run(&mut agent, "two").await;
    assert_eq!(failed.outcome, TurnOutcome::Failed);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let interrupted = agent.run_turn("three", &mut |_| {}, &cancel).await;
    assert_eq!(interrupted.outcome, TurnOutcome::Interrupted);
    assert_eq!(
        agent.last_assistant_reply().as_deref(),
        Some("first answer")
    );
    run(&mut agent, "four").await;
    assert_eq!(
        agent.last_assistant_reply().as_deref(),
        Some("second answer")
    );
    agent.clear_history();
    assert_eq!(agent.last_assistant_reply().as_deref(), None);
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

type Rejected<'a> = (&'a str, &'a str, &'a str, ToolRejection, Option<&'a str>);

fn rejections(events: &[UiEvent]) -> Vec<Rejected<'_>> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ToolRejected {
                call_id,
                tool_name,
                arguments,
                reason,
                description,
                ..
            } => Some((
                call_id.as_str(),
                tool_name.as_str(),
                arguments.as_str(),
                *reason,
                description
                    .as_ref()
                    .map(|description| description.title.as_str()),
            )),
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
                provider_replay: None,
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
                provider_replay: None,
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
                provider_replay: None,
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
async fn delegations_run_in_their_own_parallel_group_apart_from_reads() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"text":"read"}"#),
            ("call-2", r#"{"delegate":1,"meet":1}"#),
            ("call-3", r#"{"delegate":2,"meet":2}"#),
            ("call-4", r#"{"text":"again"}"#),
        ]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = tokio::time::timeout(Duration::from_secs(10), run(&mut agent, "go"))
        .await
        .expect("delegations overlap");
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        dispatch_order(&events),
        [
            "start call-1",
            "finish call-1",
            "start call-2",
            "start call-3",
            "finish call-2",
            "finish call-3",
            "start call-4",
            "finish call-4",
        ]
    );
}

#[tokio::test]
async fn delegations_carry_the_root_users_requests_and_other_calls_do_not() {
    let provider = FakeProvider::new(vec![
        text_reply("noted"),
        tool_reply(&[
            ("call-1", r#"{"intent":1,"delegate":1}"#),
            ("call-2", r#"{"intent":2}"#),
        ]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    run(&mut agent, "first request").await;
    let (report, _) = run(&mut agent, "second request").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let delegated = RootUserRequests {
        current: "second request".to_owned(),
        earlier: vec!["first request".to_owned()],
        compacted_turns: None,
    };
    assert_eq!(
        provider.requests()[2].messages[4..],
        [
            tool_message(
                "call-1",
                &format!("{:?}", Some(delegated)),
                ToolResultStatus::Success
            ),
            tool_message("call-2", "None", ToolResultStatus::Success),
        ]
    );
}

#[tokio::test]
async fn a_delegation_reports_its_childs_status_before_it_finishes() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"status":1,"delegate":1}"#),
            ("call-2", r#"{"status":2}"#),
        ]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let reported: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::SubagentStatus {
                call_id, status, ..
            } => Some(format!("status {} {}", call_id.as_str(), status.model)),
            UiEvent::ToolFinished { call_id, .. } => Some(format!("finish {}", call_id.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        reported,
        [
            "status call-1 child-model",
            "finish call-1",
            "finish call-2"
        ]
    );
    assert_eq!(
        provider.requests()[1].messages[2..],
        [
            tool_message("call-1", "status published", ToolResultStatus::Success),
            tool_message("call-2", "no status sink", ToolResultStatus::Success),
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
            arguments: r#"{"path":"outside"}"#.to_owned(),
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
                provider_replay: None,
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
async fn file_mutations_are_admitted_by_their_prepared_target_instead_of_their_arguments() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            (
                "call-1",
                r#"{"creates":"outside","access":1,"serial":true}"#,
            ),
            ("call-2", r#"{"unread":1,"serial":true}"#),
            ("call-3", r#"{"text":"after","serial":true}"#),
        ]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        finished(&events),
        [
            ("call-1", ToolResultStatus::Success),
            ("call-2", ToolResultStatus::Failure),
            ("call-3", ToolResultStatus::Success),
        ]
    );
    assert_eq!(
        provider.requests()[1].messages[2..],
        [
            tool_message("call-1", "WorkspaceOrExternal", ToolResultStatus::Success),
            tool_message(
                "call-2",
                &unconfigured_hold("echo"),
                ToolResultStatus::Failure
            ),
            tool_message(
                "call-3",
                r#"echo {"text":"after","serial":true}"#,
                ToolResultStatus::Success
            ),
        ]
    );
}

#[tokio::test]
async fn commands_are_admitted_by_their_prepared_request_instead_of_their_arguments() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            (
                "call-1",
                r#"{"run_git":"outside","access":1,"serial":true}"#,
            ),
            ("call-2", r#"{"run_rm":1,"inert":true,"serial":true}"#),
            ("call-3", r#"{"command_panic":1,"serial":true}"#),
            ("call-4", r#"{"text":"after","serial":true}"#),
        ]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let messages = &provider.requests()[1].messages[2..];
    assert_eq!(
        messages[..2],
        [
            tool_message("call-1", "WorkspaceOrExternal", ToolResultStatus::Success),
            tool_message(
                "call-2",
                &unconfigured_hold("echo"),
                ToolResultStatus::Failure
            ),
        ]
    );
    assert!(
        matches!(
            &messages[2],
            ChatMessage::Tool {
                status: ToolResultStatus::Failure,
                ..
            }
        ),
        "{:?}",
        messages[2]
    );
    assert_eq!(
        messages[3],
        tool_message(
            "call-4",
            r#"echo {"text":"after","serial":true}"#,
            ToolResultStatus::Success
        )
    );
}

#[tokio::test]
async fn commands_that_need_approval_fail_the_turn_and_gates_without_a_policy_require_it() {
    for gate in [
        Arc::new(ArgumentGate) as Arc<dyn PermissionGate>,
        Arc::new(ReadOnlyGate),
    ] {
        let provider = FakeProvider::new(vec![tool_reply(&[("call-1", r#"{"stop":1}"#)])]);
        let mut agent = Agent::new(
            provider,
            vec![echo_tool()],
            Arc::new(FixedContext),
            gate,
            config(),
        );
        let (report, events) = run(&mut agent, "go").await;
        assert_eq!(
            report.failure.map(|failure| failure.code().to_owned()),
            Some("NonInteractivePermissionRequired".to_owned())
        );
        assert_eq!(dispatch_order(&events), ["start call-1"]);
    }
}

#[tokio::test]
async fn file_mutations_that_need_approval_fail_the_turn_even_when_described_as_inert() {
    let provider = FakeProvider::new(vec![tool_reply(&[(
        "call-1",
        r#"{"changes":1,"inert":true}"#,
    )])]);
    let mut agent = new_agent(provider, vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(
        report.failure,
        Some(TurnFailure::PermissionRequired(BlockedCall {
            tool_name: "echo".to_owned(),
            arguments: r#"{"changes":1,"inert":true}"#.to_owned(),
            title: "Echoing file".to_owned(),
        }))
    );
    assert!(dispatch_order(&events).is_empty());
}

#[tokio::test]
async fn file_mutations_name_their_target_only_once_admitted() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"creates":1,"serial":true}"#),
            ("call-2", r#"{"unread":1,"serial":true}"#),
            (
                "call-3",
                r#"{"unread":1,"untargeted_panic":true,"serial":true}"#,
            ),
            ("call-4", r#"{"inert":true,"serial":true}"#),
            ("call-5", r#"{"changes":1,"untargeted_panic":true}"#),
        ]),
        text_reply("never"),
    ]);
    let mut agent = new_agent(provider, vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    let started: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ToolStarted {
                call_id,
                description,
                ..
            } => Some((call_id.as_str(), description.title.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        [
            ("call-1", r#"Echoing {"creates":1,"serial":true}"#),
            ("call-2", "Echoing file"),
            (
                "call-3",
                r#"Echoing {"unread":1,"untargeted_panic":true,"serial":true}"#
            ),
            ("call-4", r#"Echoing {"inert":true,"serial":true}"#),
        ]
    );
    assert_eq!(
        report.failure,
        Some(TurnFailure::PermissionRequired(BlockedCall {
            tool_name: "echo".to_owned(),
            arguments: r#"{"changes":1,"untargeted_panic":true}"#.to_owned(),
            title: r#"Echoing {"changes":1,"untargeted_panic":true}"#.to_owned(),
        }))
    );
}

#[tokio::test]
async fn gates_without_a_file_mutation_policy_require_approval_for_mutations() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"text":"read"}"#)]),
        tool_reply(&[("call-2", r#"{"creates":1}"#)]),
    ]);
    let mut agent = Agent::new(
        provider,
        vec![echo_tool()],
        Arc::new(FixedContext),
        Arc::new(ReadOnlyGate),
        config(),
    );
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(
        report.failure.map(|failure| failure.code().to_owned()),
        Some("NonInteractivePermissionRequired".to_owned())
    );
    assert_eq!(dispatch_order(&events), ["start call-1", "finish call-1"]);
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
async fn panics_while_preparing_describing_or_inspecting_a_call_become_rejected_failures() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"prepare_panic":true}"#),
            ("call-2", r#"{"describe_panic":true}"#),
            ("call-3", r#"{"mutation_panic":true}"#),
            ("call-4", r#"{"refused":true,"refusal_panic":true}"#),
            ("call-5", r#"{"text":"after"}"#),
        ]),
        text_reply("ok"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        rejections(&events),
        [
            (
                "call-1",
                "echo",
                r#"{"prepare_panic":true}"#,
                ToolRejection::Panicked,
                None
            ),
            (
                "call-2",
                "echo",
                r#"{"describe_panic":true}"#,
                ToolRejection::Panicked,
                None
            ),
            (
                "call-3",
                "echo",
                r#"{"mutation_panic":true}"#,
                ToolRejection::Panicked,
                None
            ),
            (
                "call-4",
                "echo",
                r#"{"refused":true,"refusal_panic":true}"#,
                ToolRejection::Panicked,
                None
            ),
        ]
    );
    assert_eq!(
        provider.requests()[1].messages[2..],
        [
            tool_message("call-1", PANICKED, ToolResultStatus::Failure),
            tool_message("call-2", PANICKED, ToolResultStatus::Failure),
            tool_message("call-3", PANICKED, ToolResultStatus::Failure),
            tool_message("call-4", PANICKED, ToolResultStatus::Failure),
            tool_message(
                "call-5",
                r#"echo {"text":"after"}"#,
                ToolResultStatus::Success
            ),
        ]
    );
}

#[tokio::test]
async fn calls_whose_drop_panics_after_their_inspection_panicked_are_rejected() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"describe_panic":true,"drop_panic":true}"#),
            (
                "call-2",
                r#"{"creates":1,"mutation_panic":true,"drop_panic":true}"#,
            ),
            ("call-3", r#"{"text":"after"}"#),
        ]),
        text_reply("ok"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let reasons: Vec<(&str, ToolRejection)> = rejections(&events)
        .into_iter()
        .map(|(call_id, _, _, reason, _)| (call_id, reason))
        .collect();
    assert_eq!(
        reasons,
        [
            ("call-1", ToolRejection::Panicked),
            ("call-2", ToolRejection::Panicked)
        ]
    );
    assert_eq!(
        provider.requests()[1].messages[2..],
        [
            tool_message("call-1", PANICKED, ToolResultStatus::Failure),
            tool_message("call-2", PANICKED, ToolResultStatus::Failure),
            tool_message(
                "call-3",
                r#"echo {"text":"after"}"#,
                ToolResultStatus::Success
            ),
        ]
    );
}

#[tokio::test]
async fn held_calls_are_dropped_without_letting_a_panic_escape() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"unread":1,"drop_panic":true}"#),
            ("call-2", r#"{"text":"after"}"#),
        ]),
        text_reply("ok"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        provider.requests()[1].messages[2..],
        [
            tool_message(
                "call-1",
                &unconfigured_hold("echo"),
                ToolResultStatus::Failure
            ),
            tool_message(
                "call-2",
                r#"echo {"text":"after"}"#,
                ToolResultStatus::Success
            ),
        ]
    );
}

#[tokio::test]
async fn a_panic_before_execute_returns_its_future_fails_only_that_call() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"text":"before"}"#),
            ("call-2", r#"{"execute_panic":true}"#),
            ("call-3", r#"{"text":"after"}"#),
        ]),
        text_reply("ok"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        finished(&events),
        [
            ("call-1", ToolResultStatus::Success),
            ("call-2", ToolResultStatus::Failure),
            ("call-3", ToolResultStatus::Success),
        ]
    );
    assert_eq!(
        provider.requests()[1].messages[2..],
        [
            tool_message(
                "call-1",
                r#"echo {"text":"before"}"#,
                ToolResultStatus::Success
            ),
            tool_message("call-2", PANICKED, ToolResultStatus::Failure),
            tool_message(
                "call-3",
                r#"echo {"text":"after"}"#,
                ToolResultStatus::Success
            ),
        ]
    );
}

#[tokio::test]
async fn panic_payloads_that_panic_when_dropped_stay_contained() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"describe_bomb":true}"#),
            ("call-2", r#"{"execute_bomb":true}"#),
            ("call-3", r#"{"text":"after"}"#),
        ]),
        text_reply("ok"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        finished(&events),
        [
            ("call-2", ToolResultStatus::Failure),
            ("call-3", ToolResultStatus::Success),
        ]
    );
    assert_eq!(
        provider.requests()[1].messages[2..],
        [
            tool_message("call-1", PANICKED, ToolResultStatus::Failure),
            tool_message("call-2", PANICKED, ToolResultStatus::Failure),
            tool_message(
                "call-3",
                r#"echo {"text":"after"}"#,
                ToolResultStatus::Success
            ),
        ]
    );
}

#[tokio::test]
async fn calls_are_found_by_the_spec_read_when_the_agent_was_built() {
    let reads = Arc::new(AtomicUsize::new(0));
    let tool: Arc<dyn Tool> = Arc::new(SpecReadOnce {
        inner: echo_tool(),
        reads: Arc::clone(&reads),
    });
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"text":"found"}"#)]),
        text_reply("ok"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![tool]);
    let (report, _) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider.requests()[1].messages[2..],
        [tool_message(
            "call-1",
            r#"echo {"text":"found"}"#,
            ToolResultStatus::Success
        )]
    );
}

#[tokio::test]
async fn calls_blocked_by_approval_are_dropped_without_letting_a_panic_escape() {
    let cases = [
        [
            r#"{"path":"outside","drop_panic":true}"#,
            r#"{"text":"later"}"#,
            r#"{"serial":true}"#,
        ],
        [
            r#"{"path":"outside"}"#,
            r#"{"drop_panic":true}"#,
            r#"{"serial":true}"#,
        ],
        [
            r#"{"path":"outside"}"#,
            r#"{"text":"later"}"#,
            r#"{"serial":true,"drop_panic":true}"#,
        ],
    ];
    for [blocked, later, carried] in cases {
        let provider = FakeProvider::new(vec![tool_reply(&[
            ("call-1", r#"{"text":"before"}"#),
            ("call-2", blocked),
            ("call-3", later),
            ("call-4", carried),
        ])]);
        let mut agent = new_agent(provider, vec![echo_tool()]);
        let (report, events) = run(&mut agent, "go").await;
        assert_eq!(
            report.failure,
            Some(TurnFailure::PermissionRequired(BlockedCall {
                tool_name: "echo".to_owned(),
                arguments: blocked.to_owned(),
                title: format!("Echoing {blocked}"),
            })),
            "{later} {carried}"
        );
        assert_eq!(
            dispatch_order(&events),
            ["start call-1", "start call-2", "finish call-1"]
        );
        assert_eq!(
            agent.history.last(),
            Some(&tool_message(
                "call-1",
                r#"echo {"text":"before"}"#,
                ToolResultStatus::Success
            ))
        );
    }
}

#[tokio::test(start_paused = true)]
async fn dropping_a_running_turn_drops_its_deferred_call_without_letting_a_panic_escape() {
    let provider = FakeProvider::new(vec![tool_reply(&[
        ("call-1", r#"{"hang":true}"#),
        ("call-2", r#"{"serial":true,"drop_panic":true}"#),
    ])]);
    let dropped = Arc::new(AtomicBool::new(false));
    let mut agent = new_agent(provider, vec![echo_tool_with(Arc::clone(&dropped))]);
    let cancel = CancellationToken::new();
    let mut ignore = |_| {};
    let turn = agent.run_turn("go", &mut ignore, &cancel);
    assert!(
        tokio::time::timeout(Duration::from_mins(1), turn)
            .await
            .is_err()
    );
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn calls_skipped_by_cancellation_are_dropped_without_letting_a_panic_escape() {
    let cases = [
        [
            r#"{"drop_panic":true}"#,
            r#"{"text":"later"}"#,
            r#"{"serial":true}"#,
        ],
        [
            r#"{"text":"next"}"#,
            r#"{"drop_panic":true}"#,
            r#"{"serial":true}"#,
        ],
        [
            r#"{"text":"next"}"#,
            r#"{"text":"later"}"#,
            r#"{"serial":true,"drop_panic":true}"#,
        ],
    ];
    for [next, later, carried] in cases {
        let provider = FakeProvider::new(vec![tool_reply(&[
            ("call-1", r#"{"wait":true}"#),
            ("call-2", next),
            ("call-3", later),
            ("call-4", carried),
        ])]);
        let mut agent = new_agent(provider, vec![echo_tool()]);
        let (report, events) = run_cancelled_at(&mut agent, "call-1").await;
        assert_eq!(
            report.outcome,
            TurnOutcome::Interrupted,
            "{next} {later} {carried}"
        );
        assert_eq!(dispatch_order(&events), ["start call-1", "finish call-1"]);
        assert_eq!(
            agent.history.last(),
            Some(&tool_message(
                "call-1",
                "stopped after cleanup",
                ToolResultStatus::Failure
            ))
        );
    }
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
    let waits: Vec<Option<Duration>> = recoveries(&events)
        .iter()
        .map(|status| status.retry_wait)
        .collect();
    assert_eq!(
        waits,
        [
            Some(Duration::from_millis(250)),
            None,
            Some(Duration::from_secs(2)),
            None,
            None,
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
    let mut statuses = recoveries(&events);
    let stop = statuses.pop().unwrap();
    assert!(
        statuses
            .iter()
            .all(|status| status.action == Some(ModelRecoveryAction::WaitingForConnectivity))
    );
    assert_eq!(
        statuses[0].label(),
        "⚠ Connection lost · waiting for connection · 1s"
    );
    assert!(stop.is_terminal());
    assert_eq!(stop.failed_attempt, DEFAULT_MAX_PROVIDER_ATTEMPTS);
    assert_eq!(
        stop.label(),
        "⚠ Connection lost · ConnectionFailed · stopped after 10 attempts"
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
async fn a_retried_request_that_fails_without_a_status_stops_the_recovery() {
    let mut unavailable = failure(ProviderErrorKind::ServerError, "server_error");
    unavailable.diagnostic = Some("HTTP 503 · overloaded".to_owned());
    let provider = FakeProvider::new(vec![
        Script::Fail(Vec::new(), unavailable.clone()),
        Script::Fail(
            Vec::new(),
            failure(ProviderErrorKind::ConnectionFailed, "ConnectionFailed"),
        ),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    let labels: Vec<String> = recoveries(&events)
        .iter()
        .map(RouteRecoveryStatus::label)
        .collect();
    assert_eq!(
        labels.last().map(String::as_str),
        Some("⚠ Provider unavailable · ConnectionFailed · stopped after 2 attempts")
    );
    let mut rejected = failure(ProviderErrorKind::InvalidRequest, "invalid_request");
    rejected.status = Some(400);
    let provider = FakeProvider::new(vec![
        Script::Fail(Vec::new(), unavailable),
        Script::Fail(Vec::new(), rejected),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert!(
        recoveries(&events)
            .iter()
            .all(|status| !status.is_terminal())
    );
}

#[tokio::test(start_paused = true)]
async fn a_retried_request_refused_before_admission_publishes_no_in_flight_status() {
    let mut unavailable = failure(ProviderErrorKind::ServerError, "server_error");
    unavailable.diagnostic = Some("HTTP 503 · overloaded".to_owned());
    let provider = FakeProvider::new(vec![
        Script::Fail(Vec::new(), unavailable),
        Script::Refuse(failure(
            ProviderErrorKind::ProviderError,
            "InvalidChatGptSubscriptionAccount",
        )),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    let labels: Vec<String> = recoveries(&events)
        .iter()
        .map(RouteRecoveryStatus::label)
        .collect();
    assert_eq!(
        labels,
        [
            "⚠ Provider unavailable · HTTP 503 · overloaded · retrying request",
            "⚠ Provider unavailable · InvalidChatGptSubscriptionAccount · stopped after 1 attempt",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_retried_request_shows_its_status_once_it_is_admitted() {
    let mut unavailable = failure(ProviderErrorKind::ServerError, "server_error");
    unavailable.diagnostic = Some("HTTP 503 · overloaded".to_owned());
    let provider = FakeProvider::new(vec![
        Script::Fail(Vec::new(), unavailable),
        text_reply("ok"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (_, events) = run(&mut agent, "go").await;
    let in_flight = events
        .iter()
        .position(
            |event| matches!(event, UiEvent::Recovery { status, .. } if status.failed_attempt == 2),
        )
        .unwrap();
    let reply = events
        .iter()
        .position(|event| matches!(event, UiEvent::AssistantText { .. }))
        .unwrap();
    assert!(in_flight < reply);
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
                provider_replay: None,
            },
            ChatMessage::user("two"),
        ]
    );
}

fn started_turn(events: &[UiEvent]) -> TurnId {
    events
        .iter()
        .find_map(|event| match event {
            UiEvent::TurnStarted { turn_id } => Some(*turn_id),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn reconfigured_models_apply_to_later_turns_and_keep_history() {
    let provider = FakeProvider::new(vec![text_reply("first"), text_reply("second")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    run(&mut agent, "one").await;
    agent.set_config(AgentConfig {
        model: "next-model".to_owned(),
        max_output_tokens: Some(128),
        ..config()
    });
    run(&mut agent, "two").await;
    let requests = provider.requests();
    assert_eq!(requests[0].model, "test-model");
    assert_eq!(requests[1].model, "next-model");
    assert_eq!(requests[1].max_output_tokens, Some(128));
    assert_eq!(requests[1].messages.len(), 3);
}

#[tokio::test]
async fn clearing_history_starts_fresh_but_keeps_turn_ids_unique() {
    let provider = FakeProvider::new(vec![text_reply("first"), text_reply("second")]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    let (_, first) = run(&mut agent, "one").await;
    agent.clear_history();
    let (_, second) = run(&mut agent, "two").await;
    assert_ne!(started_turn(&first), started_turn(&second));
    assert_eq!(
        provider.requests()[1].messages,
        vec![ChatMessage::user("two")]
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

#[tokio::test]
async fn assistant_messages_carry_their_completion_replay_into_later_requests() {
    let provider = FakeProvider::new(vec![
        with_replay(tool_reply(&[("call-1", "{}")]), "tool step"),
        with_replay(text_reply("Answer."), "answer"),
        text_reply("Again."),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    assert_eq!(run(&mut agent, "go").await.0.final_text, "Answer.");
    assert_eq!(run(&mut agent, "more").await.0.final_text, "Again.");
    let requests = provider.requests();
    assert_eq!(replays(&requests[1].messages), [Some("tool step")]);
    assert_eq!(
        replays(&requests[2].messages),
        [Some("tool step"), Some("answer")]
    );
    assert!(provider.projections().is_empty());
}

#[tokio::test]
async fn identical_answers_keep_their_own_replay() {
    let provider = FakeProvider::new(vec![
        with_replay(text_reply("OK"), "first reasoning"),
        with_replay(text_reply("OK"), "second reasoning"),
        text_reply("Done."),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    for prompt in ["one", "two", "three"] {
        run(&mut agent, prompt).await;
    }
    assert_eq!(
        replays(&provider.requests()[2].messages),
        [Some("first reasoning"), Some("second reasoning")]
    );
}

#[tokio::test]
async fn empty_answers_keep_only_the_reasoning_part_of_their_replay() {
    let provider = FakeProvider::new(vec![
        with_replay(text_reply(" "), "parts"),
        text_reply("Next."),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    assert_eq!(
        run(&mut agent, "go").await.0.final_text,
        EMPTY_RESPONSE_TEXT
    );
    run(&mut agent, "next").await;
    assert_eq!(provider.projections(), [("parts".to_owned(), false, true)]);
    assert_eq!(
        provider.requests()[1].messages[1],
        ChatMessage::Assistant {
            content: Some(EMPTY_RESPONSE_TEXT.to_owned()),
            tool_calls: Vec::new(),
            provider_replay: Some(replay("reasoning of parts")),
        }
    );
}

#[tokio::test]
async fn replay_projection_failures_fail_the_turn() {
    let provider = FakeProvider::new(vec![with_replay(text_reply(""), "invalid")]);
    let mut agent = new_agent(provider, Vec::new());
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Failed);
    assert_eq!(
        report.failure,
        Some(TurnFailure::Provider(failure(
            ProviderErrorKind::Protocol,
            "InvalidProviderState"
        )))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, UiEvent::Operational { .. }))
    );
}

#[tokio::test]
async fn a_silent_answer_keeps_its_replay_ahead_of_the_summary_prompt() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        tool_reply(&[("call-2", "{}")]),
        with_replay(text_reply(""), "silent"),
        text_reply("Summary."),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    assert_eq!(run(&mut agent, "go").await.0.final_text, "Summary.");
    let messages = &provider.requests()[3].messages;
    assert_eq!(
        messages[messages.len() - 2..],
        [
            ChatMessage::Assistant {
                content: Some(String::new()),
                tool_calls: Vec::new(),
                provider_replay: Some(replay("silent")),
            },
            ChatMessage::user(SUMMARIZE_PROMPT),
        ]
    );
}

#[tokio::test]
async fn interrupted_tool_steps_keep_their_replay_when_every_call_finished() {
    let provider = FakeProvider::new(vec![with_replay(
        tool_reply(&[
            ("call-1", r#"{"text":"before"}"#),
            ("call-2", r#"{"wait":true}"#),
        ]),
        "complete",
    )]);
    let mut agent = new_agent(provider, vec![echo_tool()]);
    let (report, _) = run_cancelled_at(&mut agent, "call-2").await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(replays(&agent.history), [Some("complete")]);
}

#[tokio::test(start_paused = true)]
async fn interrupted_tool_steps_drop_their_replay_when_a_call_is_dropped() {
    let provider = FakeProvider::new(vec![with_replay(
        tool_reply(&[
            ("call-1", r#"{"text":"before"}"#),
            ("call-2", r#"{"hang":true}"#),
        ]),
        "partial",
    )]);
    let mut agent = new_agent(provider, vec![echo_tool()]);
    let (report, _) = run_cancelled_at(&mut agent, "call-2").await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(
        agent.history[1],
        ChatMessage::Assistant {
            content: None,
            tool_calls: vec![echo_call("call-1", r#"{"text":"before"}"#)],
            provider_replay: None,
        }
    );
}

#[tokio::test]
async fn an_interrupted_summary_keeps_the_replay_only_answer() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", "{}")]),
        tool_reply(&[("call-2", "{}")]),
        with_replay(text_reply(""), "silent"),
    ]);
    let mut agent = new_agent(provider, vec![echo_tool()]);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let mut usage_reports = 0;
    let report = agent
        .run_turn(
            "go",
            &mut |event| {
                if matches!(event, UiEvent::UsageReported { .. }) {
                    usage_reports += 1;
                    if usage_reports == 3 {
                        trigger.cancel();
                    }
                }
            },
            &cancel,
        )
        .await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(
        agent.history[agent.history.len() - 2..],
        [
            ChatMessage::Assistant {
                content: Some(String::new()),
                tool_calls: Vec::new(),
                provider_replay: Some(replay("silent")),
            },
            ChatMessage::user(SUMMARIZE_PROMPT),
        ]
    );
}

mod approvals;
mod capabilities;
mod compaction;
mod malformed_arguments;
mod project_context;
mod recovery;
mod recovery_pause;
mod response_language;
mod reviews;
mod skills;
mod steering;
mod turn_log;

fn unconfigured_hold(tool_name: &str) -> String {
    tool_review_held_json(
        tool_name,
        ReviewHold::Unavailable(ReviewFailure::ReviewerUnconfigured),
    )
}

struct SwitchedTools {
    generation: AtomicUsize,
    tools: Mutex<Vec<Arc<dyn Tool>>>,
    notices: Mutex<Vec<String>>,
}

impl DynamicTools for SwitchedTools {
    fn generation(&self) -> u64 {
        u64::try_from(self.generation.load(Ordering::SeqCst)).unwrap()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.lock().unwrap().clone()
    }

    fn take_notices(&self) -> Vec<String> {
        mem::take(&mut self.notices.lock().unwrap())
    }
}

#[tokio::test]
async fn dynamic_tools_are_advertised_from_the_step_after_they_change() {
    let provider = FakeProvider::new(vec![
        text_reply("none yet"),
        tool_reply(&[("call-1", r#"{"text":"a"}"#)]),
        text_reply("done"),
    ]);
    let source = Arc::new(SwitchedTools {
        generation: AtomicUsize::new(0),
        tools: Mutex::new(Vec::new()),
        notices: Mutex::new(Vec::new()),
    });
    let mut agent =
        new_agent(Arc::clone(&provider), Vec::new()).with_dynamic_tools(Arc::clone(&source) as _);
    run(&mut agent, "first").await;
    source.tools.lock().unwrap().push(echo_tool());
    source.generation.store(1, Ordering::SeqCst);
    let (report, events) = run(&mut agent, "second").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = provider.requests();
    assert!(requests[0].tools.is_empty());
    let names: Vec<_> = requests[1]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(names, ["echo"]);
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::ToolFinished {
            status: ToolResultStatus::Success,
            ..
        }
    )));
}

#[tokio::test]
async fn notices_from_a_dynamic_tool_refresh_are_reported_once_in_the_turn() {
    let provider = FakeProvider::new(vec![text_reply("first"), text_reply("second")]);
    let source = Arc::new(SwitchedTools {
        generation: AtomicUsize::new(1),
        tools: Mutex::new(vec![echo_tool()]),
        notices: Mutex::new(vec!["[context] MCP schema \"big\" rejected".to_owned()]),
    });
    let mut agent =
        new_agent(Arc::clone(&provider), Vec::new()).with_dynamic_tools(Arc::clone(&source) as _);
    let notices = |events: &[UiEvent]| -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                UiEvent::ContextNotice { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    };
    let (_, events) = run(&mut agent, "first").await;
    assert_eq!(notices(&events), ["[context] MCP schema \"big\" rejected"]);
    let (_, events) = run(&mut agent, "second").await;
    assert!(notices(&events).is_empty());
}
