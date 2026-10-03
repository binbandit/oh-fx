use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use ofx_contract::{
    Admission, ApplicableTarget, AutoCompactPercent, ChatMessage, Completion, FinishReason,
    ModelProvider, ModelRequest, PathAccess, PermissionGate, ProviderError, ProviderErrorKind,
    StreamEvent, StreamSink, SubagentRequestInput, ToolCall, ToolCallId, ToolResultStatus, Usage,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::orchestrator::{AgentConfig, RuntimeContext};

const BASE_PROMPT: &str = "base prompt";

enum Script {
    Reply(&'static str),
    Fail(ProviderError),
    Hold,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    model: String,
    effort: Option<String>,
    system_prompt: String,
    messages: Vec<ChatMessage>,
}

#[derive(Default)]
struct Provider {
    scripts: Mutex<VecDeque<Script>>,
    seen: Mutex<Vec<Seen>>,
    holding: Notify,
}

impl Provider {
    fn new(scripts: Vec<Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            ..Self::default()
        })
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl ModelProvider for Provider {
    fn stream<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        sink: &'a mut dyn StreamSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        self.seen.lock().unwrap().push(Seen {
            model: request.model.to_owned(),
            effort: request.provider_options.reasoning_effort.map(str::to_owned),
            system_prompt: request.instructions[0].to_owned(),
            messages: request.messages.to_vec(),
        });
        let script = self.scripts.lock().unwrap().pop_front();
        Box::pin(async move {
            match script {
                Some(Script::Reply(text)) => {
                    sink.emit(StreamEvent::TextDelta {
                        text: text.to_owned(),
                    });
                    Ok(Completion {
                        content: Some(text.to_owned()),
                        tool_calls: Vec::new(),
                        finish_reason: FinishReason::Stop,
                        usage: Usage::default(),
                        provider_replay: None,
                    })
                }
                Some(Script::Fail(error)) => {
                    sink.emit(StreamEvent::TextDelta {
                        text: "partial answer".to_owned(),
                    });
                    Err(error)
                }
                Some(Script::Hold) => {
                    self.holding.notify_one();
                    cancel.cancelled().await;
                    Err(ProviderError::cancelled())
                }
                None => Err(ProviderError::new(ProviderErrorKind::Protocol, "NoScript")),
            }
        })
    }
}

struct NoContext;

impl RuntimeContext for NoContext {
    fn runtime_context(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
}

struct AllowAll;

impl PermissionGate for AllowAll {
    fn admit(&self, _call: &ToolCall) -> Admission {
        Admission::Allowed(PathAccess::WorkspaceOnly)
    }

    fn applicable_target(&self, _call: &ToolCall) -> Option<ApplicableTarget> {
        None
    }

    fn forget_approvals(&self) {}
}

struct Agents {
    provider: Arc<Provider>,
    created: Mutex<Vec<(ChildSettings, LivePermissionMode)>>,
}

impl ChildAgents for Agents {
    fn defaults(&self) -> ChildDefaults {
        ChildDefaults {
            settings: ChildSettings {
                model: "parent-model".to_owned(),
                effort: ReasoningEffort::Auto,
                fast_mode: false,
            },
            permission_mode: PermissionMode::Auto,
        }
    }

    fn agent(&self, settings: &ChildSettings, permission_mode: LivePermissionMode) -> Agent {
        self.created
            .lock()
            .unwrap()
            .push((settings.clone(), permission_mode));
        Agent::new(
            Arc::clone(&self.provider) as Arc<dyn ModelProvider>,
            Vec::new(),
            Arc::new(NoContext),
            Arc::new(AllowAll),
            AgentConfig {
                model: settings.model.clone(),
                system_prompt: BASE_PROMPT.to_owned(),
                max_output_tokens: None,
                step_limit: 0,
                reasoning_effort: settings.effort.clone().into_named(),
                fast_mode: settings.fast_mode,
                auto_compact_percent: AutoCompactPercent::resolve(None, None),
            },
        )
    }
}

struct Harness {
    provider: Arc<Provider>,
    agents: Arc<Agents>,
    host: SubagentHost,
}

impl Harness {
    fn new(scripts: Vec<Script>) -> Self {
        let provider = Provider::new(scripts);
        let agents = Arc::new(Agents {
            provider: Arc::clone(&provider),
            created: Mutex::new(Vec::new()),
        });
        Self {
            host: SubagentHost::new(Arc::clone(&agents) as Arc<dyn ChildAgents>),
            provider,
            agents,
        }
    }

    fn call(
        &self,
        call_id: &str,
        input: SubagentRequestInput<'_>,
        cancel: &CancellationToken,
    ) -> BoxFuture<'static, ToolOutput> {
        self.host.execute(
            SubagentRequest::validate(input).unwrap(),
            ToolContext::new(
                ToolCallId::new(call_id),
                cancel.clone(),
                PathAccess::WorkspaceOnly,
            ),
        )
    }

    async fn run(&self, call_id: &str, input: SubagentRequestInput<'_>) -> ToolOutput {
        self.call(call_id, input, &CancellationToken::new()).await
    }
}

fn run(task: &str) -> SubagentRequestInput<'_> {
    SubagentRequestInput::Run {
        task,
        model: None,
        effort: None,
    }
}

fn message<'a>(
    agent: &'a str,
    instructions: Option<&'a str>,
    text: &'a str,
) -> SubagentRequestInput<'a> {
    SubagentRequestInput::Message {
        agent,
        instructions,
        message: text,
        model: None,
        effort: None,
    }
}

fn succeeded(result: &str) -> ToolOutput {
    ToolOutput::success(format!(
        r#"{{"ok":true,"result":"{result}","error_code":null}}"#
    ))
}

fn rejected(code: &str) -> ToolOutput {
    ToolOutput::failure(format!(
        r#"{{"ok":false,"result":null,"error_code":"{code}"}}"#
    ))
}

#[tokio::test]
async fn a_run_returns_the_childs_reply_from_a_fresh_conversation() {
    let harness = Harness::new(vec![Script::Reply("child done"), Script::Reply("again")]);
    assert_eq!(
        harness.run("call-1", run("inspect auth")).await,
        succeeded("child done")
    );
    assert_eq!(
        harness.run("call-2", run("inspect again")).await,
        succeeded("again")
    );
    let seen = harness.provider.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].model, "parent-model");
    assert_eq!(seen[0].effort, None);
    assert_eq!(seen[0].system_prompt, BASE_PROMPT);
    assert_eq!(seen[0].messages, vec![ChatMessage::user("inspect auth")]);
    assert_eq!(seen[1].messages, vec![ChatMessage::user("inspect again")]);
    let created = harness.agents.created.lock().unwrap();
    assert_eq!(created.len(), 2);
    assert!(
        created
            .iter()
            .all(|(_, mode)| mode.get() == PermissionMode::Auto)
    );
}

#[tokio::test]
async fn a_named_agent_keeps_its_conversation_and_instruction_overlay() {
    let harness = Harness::new(vec![
        Script::Reply("first"),
        Script::Reply("second"),
        Script::Reply("third"),
    ]);
    assert_eq!(
        harness
            .run(
                "call-1",
                message("reviewer", Some("Review strictly."), "look at a")
            )
            .await,
        succeeded("first")
    );
    assert_eq!(
        harness
            .run("call-2", message("reviewer", None, "look at b"))
            .await,
        succeeded("second")
    );
    assert_eq!(
        harness
            .run(
                "call-3",
                message("reviewer", Some("Audit security."), "look at c")
            )
            .await,
        succeeded("third")
    );
    let seen = harness.provider.seen();
    let overlay = |text: &str| {
        format!("{BASE_PROMPT}\n\n<subagent_instructions>\n{text}\n</subagent_instructions>")
    };
    assert_eq!(seen[0].system_prompt, overlay("Review strictly."));
    assert_eq!(seen[1].system_prompt, overlay("Review strictly."));
    assert_eq!(seen[2].system_prompt, overlay("Audit security."));
    assert_eq!(
        seen[1].messages,
        vec![
            ChatMessage::user("look at a"),
            ChatMessage::Assistant {
                content: Some("first".to_owned()),
                tool_calls: Vec::new(),
                provider_replay: None,
            },
            ChatMessage::user("look at b"),
        ]
    );
    assert_eq!(seen[2].messages.len(), 5);
    assert_eq!(harness.agents.created.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn creation_overrides_route_new_children_and_are_rejected_for_existing_ones() {
    let harness = Harness::new(vec![Script::Reply("routed"), Script::Reply("named")]);
    assert_eq!(
        harness
            .run(
                "call-1",
                SubagentRequestInput::Run {
                    task: "probe",
                    model: Some("other-model"),
                    effort: Some("high"),
                },
            )
            .await,
        succeeded("routed")
    );
    assert_eq!(
        harness
            .run("call-2", message("reviewer", None, "hello"))
            .await,
        succeeded("named")
    );
    assert_eq!(
        harness
            .run(
                "call-3",
                SubagentRequestInput::Message {
                    agent: "reviewer",
                    instructions: None,
                    message: "again",
                    model: None,
                    effort: Some("low"),
                },
            )
            .await,
        rejected("override_after_create")
    );
    let seen = harness.provider.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].model, "other-model");
    assert_eq!(seen[1].model, "parent-model");
    let created = harness.agents.created.lock().unwrap();
    let settings: Vec<&ChildSettings> = created.iter().map(|(settings, _)| settings).collect();
    assert_eq!(
        settings,
        [
            &ChildSettings {
                model: "other-model".to_owned(),
                effort: ReasoningEffort::parse("high").unwrap(),
                fast_mode: false,
            },
            &ChildSettings {
                model: "parent-model".to_owned(),
                effort: ReasoningEffort::Auto,
                fast_mode: false,
            },
        ]
    );
}

#[tokio::test]
async fn a_repeated_operation_replays_its_result_and_a_changed_one_conflicts() {
    let harness = Harness::new(vec![Script::Reply("once")]);
    assert_eq!(harness.run("call-1", run("task")).await, succeeded("once"));
    assert_eq!(harness.run("call-1", run("task")).await, succeeded("once"));
    assert_eq!(
        harness.run("call-1", run("another task")).await,
        rejected("operation_conflict")
    );
    assert_eq!(harness.provider.seen().len(), 1);
}

#[tokio::test]
async fn a_failed_child_reports_its_cause_and_partial_result() {
    let mut error = ProviderError::new(ProviderErrorKind::Protocol, "HttpStatus");
    error.status = Some(500);
    error.diagnostic = Some("HTTP 500 · boom".to_owned());
    let harness = Harness::new(vec![Script::Fail(error)]);
    assert_eq!(
        harness.run("call-1", run("task")).await,
        ToolOutput::failure(
            SubagentResult {
                result: Some(
                    "Subagent failed: provider_http_error: API request failed · HTTP 500 · boom. Earlier tool calls may have completed; their effects are not rolled back.\n\nPartial result:\npartial answer"
                ),
                ..SubagentResult::failure("child_failed")
            }
            .encode()
        )
    );
}

#[tokio::test]
async fn a_cancelled_parent_cancels_its_child_and_a_busy_child_refuses_new_work() {
    let harness = Harness::new(vec![Script::Hold, Script::Reply("after cancel")]);
    let cancel = CancellationToken::new();
    let first = tokio::spawn(harness.call("call-1", message("reviewer", None, "slow"), &cancel));
    harness.provider.holding.notified().await;
    assert_eq!(
        harness
            .run("call-2", message("reviewer", None, "steer"))
            .await,
        rejected("child_busy")
    );
    cancel.cancel();
    assert_eq!(first.await.unwrap(), rejected("child_cancelled"));
    let after = tokio::time::timeout(
        Duration::from_secs(5),
        harness.run("call-3", message("reviewer", None, "next")),
    )
    .await
    .unwrap();
    assert_eq!(after, succeeded("after cancel"));
    assert_eq!(
        harness
            .run("call-3", message("reviewer", None, "next"))
            .await,
        succeeded("after cancel")
    );
    assert_eq!(harness.provider.seen().len(), 2);
}

#[test]
fn internal_operation_identity_is_deterministic_and_invocation_bound() {
    let first = operation_id("call-1");
    assert_eq!(first, operation_id("call-1"));
    assert_ne!(first, operation_id("call-2"));
    let identity = Sha256::digest(b"model\0call-1");
    let epoch = u64::from_le_bytes(identity[..8].try_into().unwrap()) | 1;
    assert_eq!(
        first,
        format!(
            "fxop:2:m:{epoch}:{}",
            lowercase_hex(&Sha256::digest(b"call-1"))
        )
    );
}

#[test]
fn creation_defaults_keep_parent_values_unless_the_request_overrides_them() {
    let parent = ChildSettings {
        model: "parent-model".to_owned(),
        effort: ReasoningEffort::Auto,
        fast_mode: true,
    };
    let medium = ReasoningEffort::parse("medium").unwrap();
    for (overrides, model, effort) in [
        (
            SubagentOverride {
                model: None,
                effort: None,
            },
            "parent-model",
            ReasoningEffort::Auto,
        ),
        (
            SubagentOverride {
                model: Some("gpt-5.6-sol-fast"),
                effort: Some(&medium),
            },
            "gpt-5.6-sol-fast",
            medium.clone(),
        ),
        (
            SubagentOverride {
                model: Some("other-model"),
                effort: None,
            },
            "other-model",
            ReasoningEffort::Auto,
        ),
    ] {
        let settings = effective_settings(&parent, overrides);
        assert_eq!(settings.model, model);
        assert_eq!(settings.effort, effort);
        assert!(settings.fast_mode);
    }
}

#[test]
fn subagent_failure_result_distinguishes_runtime_cause_from_retained_partial_text() {
    let text = failed_result(
        Some("agent_turn_failed: SessionCommitFailed"),
        Some("one edit completed"),
    );
    assert!(text.contains("SessionCommitFailed"));
    assert!(text.ends_with("Partial result:\none edit completed"));
    let legacy = failed_result(None, Some(""));
    assert!(legacy.contains("failure reason unavailable"));
    assert!(!legacy.contains("Partial result:"));
}

#[test]
fn terminal_result_projects_every_managed_outcome() {
    let observed = |outcome| Observation {
        outcome,
        failure: None,
    };
    let completed = terminal_result(&observed(Some(Outcome::Completed)), Some("done"));
    assert!(completed.ok);
    assert_eq!(completed.result, Some("done"));
    assert_eq!(completed.error_code, None);
    for (outcome, code) in [
        (Outcome::Failed, "child_failed"),
        (Outcome::Cancelled, "child_cancelled"),
        (Outcome::Interrupted, "child_interrupted"),
    ] {
        let projected = terminal_result(&observed(Some(outcome)), Some("partial"));
        assert!(!projected.ok);
        assert_eq!(projected.result, Some("partial"));
        assert_eq!(projected.error_code, Some(code));
    }
    let missing = terminal_result(&observed(Some(Outcome::Completed)), None);
    assert!(!missing.ok);
    assert_eq!(missing.error_code, Some("child_result_unavailable"));
    let unknown = terminal_result(&observed(None), Some("text"));
    assert_eq!(unknown.error_code, Some("child_result_unavailable"));
    assert_eq!(
        output(SubagentResult::failure("x")).status,
        ToolResultStatus::Failure
    );
}
