use std::any::Any;
use std::collections::HashMap;
use std::mem;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Duration;

use ofx_contract::{
    Admission, ApprovalDecision, ApprovalRequest, ApprovalScope, AutoCompactPercent, BoxFuture,
    CallDescription, CapabilityLookup, CapabilityResolver, ChatMessage, CommandRequest, Completion,
    Concurrency, DEFAULT_MAX_TOOL_RESULT_BYTES, ExecutionFailure, FileMutation, FinishReason,
    GatedAction, ModelCapabilities, ModelFailureDiagnostic, ModelProvider, ModelRecoveryCause,
    ModelRequest, PathAccess, PermissionGate, PreparedCall, ProviderError, ProviderErrorKind,
    ProviderOptions, RequestId, RouteRecoveryKind, RouteRecoveryStatus, StreamEvent, Tool,
    ToolArgumentDiagnostic, ToolArgumentIntegrity, ToolCall, ToolChoice, ToolContext, ToolEffect,
    ToolOutput, ToolRejection, ToolResultStatus, ToolSpec, TurnId, TurnOutcome, UiEvent, Usage,
    format_unknown_action, malformed_tool_arguments_json, non_object_tool_arguments_json,
    prepare_model_output, review_unavailable_json, tool_execution_failure_json,
    tool_permission_denied_json,
};
use ofx_text::encode_terminal_safe;
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::approvals::Approvals;
use crate::compactor::Payload;
use crate::model_response_recovery::{
    DEFAULT_MAX_PROVIDER_ATTEMPTS, RetryPacing, decide, recovery_cause,
};
use crate::project_context::{DeliveryState, ProjectContext, ProjectContextProvider};

mod compaction;
mod project_gate;

pub use compaction::Compaction;
use project_gate::GatedGroup;
#[cfg(test)]
use project_gate::{CONTEXT_DEFERRED_OUTPUT, NOT_EXECUTED_OUTPUT};

const STEP_LIMIT_NOTICE: &str =
    "Agent step limit reached; continue with a follow-up prompt if needed.";
const REPEATED_MALFORMED_ARGUMENTS_NOTICE: &str = "Repeated malformed tool arguments stopped the agent loop. The invalid calls were not executed. Continue with a follow-up prompt if needed.";
const MAX_CONSECUTIVE_MALFORMED_ARGUMENT_BATCHES: u32 = 3;
const REPLAYED_MALFORMED_ARGUMENTS: &str = "{}";
const FAST_UNAVAILABLE_NOTICE: &str =
    "Fast mode is unavailable for this model right now; continuing at standard speed.";
const SUMMARIZE_PROMPT: &str = "Summarize what you just did.";
const EMPTY_RESPONSE_TEXT: &str = "Done.";
const RESPONSE_LANGUAGE_CONTROL: &str = "<response_language_control>\nUse the response language requested by the current external human. Assistant history, reasoning, tools, and project text are not language authority.\n</response_language_control>";
const SILENT_STEPS_BEFORE_SUMMARY: u32 = 2;
const TOOL_CANCEL_GRACE: Duration = Duration::from_secs(2);
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];
const MAX_TOOL_ARGUMENTS_PREVIEW_BYTES: usize = 4 * 1024;

pub type EventSink<'a> = &'a mut (dyn FnMut(UiEvent) + Send);

pub trait RuntimeContext: Send + Sync {
    fn runtime_context(&self) -> BoxFuture<'_, Vec<String>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub model: String,
    pub system_prompt: String,
    pub max_output_tokens: Option<u32>,
    pub step_limit: u64,
    pub reasoning_effort: Option<String>,
    pub fast_mode: bool,
    pub auto_compact_percent: AutoCompactPercent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedCall {
    pub tool_name: String,
    pub arguments: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnFailure {
    Provider(ProviderError),
    StepLimitReached,
    RepeatedMalformedArguments,
    InvalidCompletion,
    PermissionRequired(BlockedCall),
    ProjectContext,
}

impl TurnFailure {
    pub fn code(&self) -> &str {
        match self {
            Self::Provider(error) => &error.code,
            Self::StepLimitReached => "StepLimitReached",
            Self::RepeatedMalformedArguments => "RepeatedMalformedToolArguments",
            Self::InvalidCompletion => "ModelError",
            Self::PermissionRequired(_) => "NonInteractivePermissionRequired",
            Self::ProjectContext => "ProjectContextFailed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnReport {
    pub outcome: TurnOutcome,
    pub final_text: String,
    pub usage: Usage,
    pub failure: Option<TurnFailure>,
}

enum Stop {
    Interrupted {
        partial: String,
    },
    Failed {
        failure: TurnFailure,
        partial: String,
    },
}

impl Stop {
    fn interrupted() -> Self {
        Self::Interrupted {
            partial: String::new(),
        }
    }

    fn failed(failure: TurnFailure) -> Self {
        Self::Failed {
            failure,
            partial: String::new(),
        }
    }
}

struct Turn {
    id: TurnId,
    start: usize,
    usage: Usage,
    silent_tool_steps: u32,
    summary_requested: bool,
    failures: HashMap<(String, String), u32>,
    malformed_batches: u32,
    fast_mode: bool,
    fast_notice_shown: bool,
}

struct ProjectInstructions {
    provider: Arc<dyn ProjectContextProvider>,
    snapshot: Option<String>,
    deltas: Vec<String>,
    delivery: DeliveryState,
    initial: DeliveryState,
}

struct KnownCapabilities {
    model: ModelCapabilities,
    catalog_unavailable: bool,
}

pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    tools: Vec<Arc<dyn Tool>>,
    tool_specs: Vec<ToolSpec>,
    context: Arc<dyn RuntimeContext>,
    permissions: Arc<dyn PermissionGate>,
    approvals: Option<Approvals>,
    config: AgentConfig,
    capability_resolver: Option<Arc<dyn CapabilityResolver>>,
    capabilities: Option<KnownCapabilities>,
    project: Option<ProjectInstructions>,
    history: Vec<ChatMessage>,
    turn_starts: Vec<usize>,
    compacted: Option<Payload>,
    turns: u64,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        tools: Vec<Arc<dyn Tool>>,
        context: Arc<dyn RuntimeContext>,
        permissions: Arc<dyn PermissionGate>,
        config: AgentConfig,
    ) -> Self {
        let tool_specs = tools.iter().map(|tool| tool.spec().clone()).collect();
        Self {
            provider,
            tools,
            tool_specs,
            context,
            permissions,
            approvals: None,
            config,
            capability_resolver: None,
            capabilities: None,
            project: None,
            history: Vec::new(),
            turn_starts: Vec::new(),
            compacted: None,
            turns: 0,
        }
    }

    #[must_use]
    pub fn with_approvals(mut self, approvals: Approvals) -> Self {
        self.approvals = Some(approvals);
        self
    }

    #[must_use]
    pub fn with_capability_resolver(mut self, resolver: Arc<dyn CapabilityResolver>) -> Self {
        self.capability_resolver = Some(resolver);
        self
    }

    #[must_use]
    pub fn with_project_context(
        mut self,
        provider: Arc<dyn ProjectContextProvider>,
        snapshot: ProjectContext,
    ) -> Self {
        let delivery = DeliveryState::from_snapshot(&snapshot);
        self.project = Some(ProjectInstructions {
            provider,
            initial: delivery.clone(),
            delivery,
            snapshot: snapshot.content,
            deltas: Vec::new(),
        });
        self
    }

    pub fn set_config(&mut self, config: AgentConfig) {
        if config.model != self.config.model {
            self.capabilities = None;
        }
        self.config = config;
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        self.turn_starts.clear();
        self.compacted = None;
        self.permissions.forget_approvals();
        if let Some(project) = &mut self.project {
            project.deltas.clear();
            project.delivery = project.initial.clone();
        }
    }

    pub async fn run_turn(
        &mut self,
        prompt: &str,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> TurnReport {
        self.turns += 1;
        let id = TurnId::new(self.turns);
        events(UiEvent::TurnStarted { turn_id: id });
        let mut turn = Turn {
            id,
            start: self.history.len(),
            usage: Usage::default(),
            silent_tool_steps: 0,
            summary_requested: false,
            failures: HashMap::new(),
            malformed_batches: 0,
            fast_mode: self.config.fast_mode,
            fast_notice_shown: false,
        };
        self.turn_starts.push(turn.start);
        self.history.push(ChatMessage::user(prompt));
        let result = self.drive(&mut turn, events, cancel).await;
        let (outcome, final_text, failure) = match result {
            Ok(text) => (TurnOutcome::Completed, text, None),
            Err(Stop::Interrupted { partial }) => {
                self.keep_partial_turn(turn.start, &partial);
                (TurnOutcome::Interrupted, String::new(), None)
            }
            Err(Stop::Failed { failure, partial }) => {
                if partial.trim_matches(TRIMMED).is_empty()
                    && !self.has_completed_tool_steps(turn.start)
                    && failure != TurnFailure::StepLimitReached
                {
                    self.history.truncate(turn.start);
                    self.turn_starts.pop();
                } else {
                    self.keep_partial_turn(turn.start, &partial);
                }
                (TurnOutcome::Failed, String::new(), Some(failure))
            }
        };
        events(UiEvent::TurnFinished {
            turn_id: id,
            outcome,
        });
        TurnReport {
            outcome,
            final_text,
            usage: turn.usage,
            failure,
        }
    }

    async fn drive(
        &mut self,
        turn: &mut Turn,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<String, Stop> {
        if self.config.reasoning_effort.is_some() || self.config.fast_mode {
            self.resolve_capabilities(cancel).await?;
        }
        let mut step = 0;
        loop {
            if self.config.step_limit != 0 && step >= self.config.step_limit {
                return Err(self.stop_with_notice(
                    turn.id,
                    events,
                    STEP_LIMIT_NOTICE,
                    TurnFailure::StepLimitReached,
                ));
            }
            if cancel.is_cancelled() {
                return Err(Stop::interrupted());
            }
            let context = self.context.runtime_context().await;
            let deltas = self
                .project
                .as_ref()
                .map_or(0, |project| project.deltas.len());
            let mut instructions: Vec<&str> = Vec::with_capacity(context.len() + deltas + 3);
            if !self.config.system_prompt.is_empty() {
                instructions.push(&self.config.system_prompt);
            }
            if let Some(project) = &self.project {
                instructions.extend(project.snapshot.as_deref());
                instructions.extend(project.deltas.iter().map(String::as_str));
            }
            instructions.extend(context.iter().map(String::as_str));
            instructions.push(RESPONSE_LANGUAGE_CONTROL);
            let request = ModelRequest {
                model: &self.config.model,
                instructions: &instructions,
                messages: &self.history,
                tools: &self.tool_specs,
                tool_choice: ToolChoice::Auto,
                max_output_tokens: self.config.max_output_tokens,
                provider_options: self.provider_options(turn, events),
            };
            let completion = self.complete(turn, request, events, cancel).await?;
            turn.usage.accumulate(completion.usage);
            events(UiEvent::UsageReported {
                turn_id: turn.id,
                usage: completion.usage,
            });
            step += 1;
            match (completion.finish_reason, completion.tool_calls.is_empty()) {
                (FinishReason::Stop, true) => {
                    if let Some(text) = self.finish(turn, completion, events)? {
                        return Ok(text);
                    }
                }
                (FinishReason::ToolCalls, false) => {
                    self.run_batch(turn, completion, events, cancel).await?;
                }
                _ => return Err(Stop::failed(TurnFailure::InvalidCompletion)),
            }
        }
    }

    async fn resolve_capabilities(&mut self, cancel: &CancellationToken) -> Result<(), Stop> {
        if self.capabilities.is_some() {
            return Ok(());
        }
        let lookup = match &self.capability_resolver {
            Some(resolver) => resolver.resolve(&self.config.model, cancel).await,
            None => CapabilityLookup::Resolved(ModelCapabilities::default()),
        };
        self.capabilities = Some(match lookup {
            CapabilityLookup::Resolved(model) => KnownCapabilities {
                model,
                catalog_unavailable: false,
            },
            CapabilityLookup::CatalogUnavailable => KnownCapabilities {
                model: ModelCapabilities::default(),
                catalog_unavailable: true,
            },
            CapabilityLookup::Cancelled => return Err(Stop::interrupted()),
        });
        Ok(())
    }

    fn provider_options(&self, turn: &mut Turn, events: EventSink<'_>) -> ProviderOptions<'_> {
        let Some(known) = &self.capabilities else {
            return ProviderOptions::default();
        };
        let options = known
            .model
            .provider_options(self.config.reasoning_effort.as_deref(), turn.fast_mode);
        if turn.fast_mode && !options.fast && known.catalog_unavailable && !turn.fast_notice_shown {
            turn.fast_notice_shown = true;
            events(UiEvent::Operational {
                turn_id: turn.id,
                text: format!("{FAST_UNAVAILABLE_NOTICE}\n"),
            });
        }
        options
    }

    async fn complete(
        &self,
        turn: &mut Turn,
        mut request: ModelRequest<'_>,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<Completion, Stop> {
        let turn_id = turn.id;
        let mut attempt = 1;
        let mut pacing = RetryPacing::Idle;
        let mut recovering = false;
        loop {
            let mut partial = String::new();
            let mut sink = |event: StreamEvent| match event {
                StreamEvent::TextDelta { text } => {
                    partial.push_str(&text);
                    events(UiEvent::AssistantText { turn_id, text });
                }
                StreamEvent::ReasoningDelta { text } => {
                    events(UiEvent::ReasoningText { turn_id, text });
                }
            };
            let error = match self.provider.stream(&request, &mut sink, cancel).await {
                Ok(completion) => {
                    if recovering {
                        events(UiEvent::Recovery {
                            turn_id,
                            status: recovered_status(attempt),
                        });
                    }
                    return Ok(completion);
                }
                Err(error) => error,
            };
            if error.kind == ProviderErrorKind::Cancelled || cancel.is_cancelled() {
                return Err(Stop::Interrupted { partial });
            }
            let cause = recovery_cause(error.kind).filter(|_| partial.is_empty());
            let Some(cause) = cause.filter(|_| attempt < DEFAULT_MAX_PROVIDER_ATTEMPTS) else {
                return Err(Stop::Failed {
                    failure: TurnFailure::Provider(error),
                    partial,
                });
            };
            if cause == ModelRecoveryCause::ProviderUnavailable {
                turn.fast_mode = false;
                request.provider_options.fast = false;
            }
            let retry_after = error.retry_after.map(|delay| delay.as_secs());
            let decision = decide(cause, retry_after, pacing);
            let mut status = RouteRecoveryStatus {
                kind: RouteRecoveryKind::AutoRetry,
                failed_attempt: attempt,
                succeeded_attempt: 0,
                attempt_limit: DEFAULT_MAX_PROVIDER_ATTEMPTS,
                cause: Some(cause),
                action: Some(decision.action),
                delay_seconds: decision.delay.as_secs(),
                diagnostic: Some(ModelFailureDiagnostic::new(
                    error.diagnostic.as_deref().unwrap_or(&error.code),
                )),
            };
            events(UiEvent::Recovery {
                turn_id,
                status: status.clone(),
            });
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(Stop::interrupted()),
                () = tokio::time::sleep(decision.delay) => {}
            }
            attempt += 1;
            status.failed_attempt = attempt;
            events(UiEvent::Recovery { turn_id, status });
            pacing = decision.next_pacing;
            recovering = true;
        }
    }

    async fn run_batch(
        &mut self,
        turn: &mut Turn,
        completion: Completion,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), Stop> {
        turn.silent_tool_steps = if completion
            .content
            .as_deref()
            .is_some_and(|text| !text.is_empty())
        {
            0
        } else {
            turn.silent_tool_steps + 1
        };
        let (calls, mut malformed) = self.record_tool_step(completion);
        let all_malformed = !malformed.is_empty() && malformed.iter().all(Option::is_some);
        let mut gate = match self.project {
            Some(_) => Some(self.open_gate(turn.id, &calls, &mut malformed, events, cancel)?),
            None => None,
        };
        let mut next = 0;
        let mut carried = Deferred(None);
        while next < calls.len() {
            if cancel.is_cancelled() {
                return Err(Stop::interrupted());
            }
            let group = match &mut gate {
                Some(gate) => match self.gated_group(gate, &calls, next) {
                    GatedGroup::Run(group) => group,
                    GatedGroup::Unexecuted(description, output) => {
                        self.settle_unexecuted(turn.id, &calls[next], description, output, events);
                        next += 1;
                        continue;
                    }
                },
                None => self.lazy_group(&calls, next, &mut malformed, &mut carried),
            };
            next += group.len();
            let gate = Gate {
                permissions: &*self.permissions,
                approvals: self.approvals.as_ref(),
            };
            let settled = run_group(turn.id, group, gate, events, cancel).await;
            for Settled {
                call,
                output,
                escalates,
            } in settled.outcomes
            {
                let Some(output) = output else {
                    continue;
                };
                let status = output.status;
                let model_output =
                    prepare_model_output(&call.name, output.content, DEFAULT_MAX_TOOL_RESULT_BYTES);
                let content = if escalates {
                    escalate_repeated_failure(turn, call, status, model_output)
                } else {
                    model_output
                };
                self.history.push(ChatMessage::Tool {
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    content,
                    status,
                });
            }
            if let Some(blocked) = settled.blocked {
                return Err(Stop::failed(TurnFailure::PermissionRequired(blocked)));
            }
        }
        if cancel.is_cancelled() {
            return Err(Stop::interrupted());
        }
        turn.malformed_batches = if all_malformed {
            turn.malformed_batches + 1
        } else {
            0
        };
        if turn.malformed_batches == MAX_CONSECUTIVE_MALFORMED_ARGUMENT_BATCHES {
            return Err(self.stop_with_notice(
                turn.id,
                events,
                REPEATED_MALFORMED_ARGUMENTS_NOTICE,
                TurnFailure::RepeatedMalformedArguments,
            ));
        }
        Ok(())
    }

    fn record_tool_step(
        &mut self,
        completion: Completion,
    ) -> (Vec<ToolCall>, Vec<Option<ToolOutput>>) {
        let malformed: Vec<Option<ToolOutput>> = completion
            .tool_calls
            .iter()
            .map(argument_rejection)
            .collect();
        let calls: Vec<ToolCall> = completion
            .tool_calls
            .into_iter()
            .zip(&malformed)
            .map(|(call, rejection)| match rejection {
                Some(_) => ToolCall {
                    arguments: REPLAYED_MALFORMED_ARGUMENTS.to_owned(),
                    ..call
                },
                None => call,
            })
            .collect();
        let history_calls = calls
            .iter()
            .zip(&malformed)
            .map(|(call, rejection)| match rejection {
                Some(_) => call.clone(),
                None => self.history_call(call.clone()),
            })
            .collect();
        self.history.push(ChatMessage::Assistant {
            content: completion.content,
            tool_calls: history_calls,
            provider_replay: completion.provider_replay,
        });
        (calls, malformed)
    }

    fn stop_with_notice(
        &mut self,
        turn_id: TurnId,
        events: EventSink<'_>,
        notice: &str,
        failure: TurnFailure,
    ) -> Stop {
        events(UiEvent::Operational {
            turn_id,
            text: format!("{notice}\n"),
        });
        self.history.push(ChatMessage::Assistant {
            content: Some(notice.to_owned()),
            tool_calls: Vec::new(),
            provider_replay: None,
        });
        Stop::failed(failure)
    }

    fn tool(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools
            .iter()
            .zip(&self.tool_specs)
            .find_map(|(tool, spec)| (spec.name == name).then_some(tool))
    }

    fn history_call(&self, call: ToolCall) -> ToolCall {
        let rewritten = self
            .tool(&call.name)
            .and_then(|tool| contained(|| tool.history_arguments(&call.arguments)).flatten());
        match rewritten {
            Some(arguments) => ToolCall { arguments, ..call },
            None => call,
        }
    }

    fn lazy_group<'c>(
        &self,
        calls: &'c [ToolCall],
        start: usize,
        malformed: &mut [Option<ToolOutput>],
        carried: &mut Deferred,
    ) -> Vec<(&'c ToolCall, Prepared)> {
        let head = carried
            .0
            .take()
            .unwrap_or_else(|| self.prepare(&calls[start], malformed[start].take()));
        let parallel = head.is_parallel();
        let mut group = vec![(&calls[start], head)];
        for (call, malformed) in calls[start + 1..].iter().zip(&mut malformed[start + 1..]) {
            if !parallel {
                break;
            }
            let prepared = self.prepare(call, malformed.take());
            if !prepared.is_parallel() {
                carried.0 = Some(prepared);
                break;
            }
            group.push((call, prepared));
        }
        group
    }

    fn prepare(&self, call: &ToolCall, malformed: Option<ToolOutput>) -> Prepared {
        match self.prepared_call(call, malformed) {
            Ok(prepared) => completed(prepared, &call.name),
            Err(rejection) => Prepared::Rejected(rejection),
        }
    }

    fn prepare_uncompleted(&self, call: &ToolCall, malformed: Option<ToolOutput>) -> Prepared {
        match self.prepared_call(call, malformed) {
            Ok(prepared) => inspected(prepared, &call.name),
            Err(rejection) => Prepared::Rejected(rejection),
        }
    }

    fn prepared_call(
        &self,
        call: &ToolCall,
        malformed: Option<ToolOutput>,
    ) -> Result<Box<dyn PreparedCall>, Rejection> {
        if let Some(output) = malformed {
            return Err(Rejection {
                reason: ToolRejection::MalformedArguments,
                title: None,
                output,
            });
        }
        let Some(tool) = self.tool(&call.name) else {
            return Err(Rejection {
                reason: ToolRejection::Unsupported,
                title: Some(format_unknown_action(&call.name)),
                output: ToolOutput::failure(format!("Unsupported tool: {}", call.name)),
            });
        };
        match contained(|| tool.prepare(&call.arguments)) {
            Some(Ok(prepared)) => Ok(prepared),
            Some(Err(output)) => Err(Rejection {
                reason: ToolRejection::Invalid,
                title: None,
                output,
            }),
            None => Err(Rejection::panicked(&call.name)),
        }
    }

    fn finish(
        &mut self,
        turn: &mut Turn,
        completion: Completion,
        events: EventSink<'_>,
    ) -> Result<Option<String>, Stop> {
        let has_content = completion
            .content
            .as_deref()
            .is_some_and(|text| !text.trim_matches(TRIMMED).is_empty());
        if !has_content
            && !turn.summary_requested
            && turn.silent_tool_steps >= SILENT_STEPS_BEFORE_SUMMARY
        {
            turn.summary_requested = true;
            if completion.provider_replay.is_some() {
                self.history.push(ChatMessage::Assistant {
                    content: completion.content,
                    tool_calls: Vec::new(),
                    provider_replay: completion.provider_replay,
                });
            }
            self.history.push(ChatMessage::user(SUMMARIZE_PROMPT));
            return Ok(None);
        }
        let (history_text, history_replay) = if has_content {
            (
                completion.content.unwrap_or_default(),
                completion.provider_replay,
            )
        } else {
            let replay = completion
                .provider_replay
                .map(|replay| self.provider.project_replay(&replay, false, true))
                .transpose()
                .map_err(|error| Stop::failed(TurnFailure::Provider(error)))?
                .flatten();
            events(UiEvent::Operational {
                turn_id: turn.id,
                text: EMPTY_RESPONSE_TEXT.to_owned(),
            });
            (EMPTY_RESPONSE_TEXT.to_owned(), replay)
        };
        self.history.push(ChatMessage::Assistant {
            content: Some(history_text.clone()),
            tool_calls: Vec::new(),
            provider_replay: history_replay,
        });
        Ok(Some(history_text))
    }

    fn has_completed_tool_steps(&self, start: usize) -> bool {
        self.history[start..]
            .iter()
            .any(|message| matches!(message, ChatMessage::Tool { .. }))
    }

    fn keep_partial_turn(&mut self, start: usize, partial: &str) {
        let completed: Vec<String> = self.history[start..]
            .iter()
            .filter_map(|message| match message {
                ChatMessage::Tool { call_id, .. } => Some(call_id.as_str().to_owned()),
                _ => None,
            })
            .collect();
        for message in &mut self.history[start..] {
            if let ChatMessage::Assistant {
                tool_calls,
                provider_replay,
                ..
            } = message
            {
                let issued = tool_calls.len();
                tool_calls.retain(|call| completed.iter().any(|id| id == call.id.as_str()));
                if tool_calls.len() != issued {
                    *provider_replay = None;
                }
            }
        }
        let mut index = start;
        while index < self.history.len() {
            let empty = matches!(
                &self.history[index],
                ChatMessage::Assistant { content, tool_calls, provider_replay: None }
                    if tool_calls.is_empty() && content.as_deref().is_none_or(str::is_empty)
            );
            if empty {
                self.history.remove(index);
            } else {
                index += 1;
            }
        }
        if !partial.is_empty() {
            self.history.push(ChatMessage::Assistant {
                content: Some(partial.to_owned()),
                tool_calls: Vec::new(),
                provider_replay: None,
            });
        }
    }
}

fn argument_rejection(call: &ToolCall) -> Option<ToolOutput> {
    let content = match ToolArgumentIntegrity::classify_function_input(&call.arguments) {
        ToolArgumentIntegrity::Valid => return None,
        ToolArgumentIntegrity::NonObjectJson => non_object_tool_arguments_json(&call.name),
        ToolArgumentIntegrity::MalformedJson => malformed_tool_arguments_json(
            &call.name,
            &ToolArgumentDiagnostic::diagnose(&call.arguments),
        ),
    };
    Some(ToolOutput::failure(content))
}

fn recovered_status(attempt: usize) -> RouteRecoveryStatus {
    RouteRecoveryStatus {
        kind: RouteRecoveryKind::AutoRecovered,
        failed_attempt: 0,
        succeeded_attempt: attempt,
        attempt_limit: DEFAULT_MAX_PROVIDER_ATTEMPTS,
        cause: None,
        action: None,
        delay_seconds: 0,
        diagnostic: None,
    }
}

struct Rejection {
    reason: ToolRejection,
    title: Option<String>,
    output: ToolOutput,
}

impl Rejection {
    fn panicked(tool_name: &str) -> Self {
        Self {
            reason: ToolRejection::Panicked,
            title: None,
            output: panicked(tool_name),
        }
    }
}

fn completed(mut prepared: Box<dyn PreparedCall>, tool_name: &str) -> Prepared {
    if contained(|| prepared.complete()).is_none() {
        discard(prepared);
        return Prepared::Rejected(Rejection::panicked(tool_name));
    }
    inspected(prepared, tool_name)
}

fn inspected(prepared: Box<dyn PreparedCall>, tool_name: &str) -> Prepared {
    let inspected = contained(|| prepared.describe())
        .zip(contained(|| prepared.file_mutation().cloned()))
        .zip(contained(|| prepared.command_request().cloned()))
        .zip(contained(|| prepared.refusal().cloned()));
    let Some((((description, mutation), command), refusal)) = inspected else {
        discard(prepared);
        return Prepared::Rejected(Rejection::panicked(tool_name));
    };
    if let Some(output) = refusal {
        discard(prepared);
        return Prepared::Rejected(Rejection {
            reason: ToolRejection::Invalid,
            title: Some(description.title),
            output,
        });
    }
    Prepared::Ready(prepared, description, mutation, command)
}

enum Prepared {
    Rejected(Rejection),
    Ready(
        Box<dyn PreparedCall>,
        CallDescription,
        Option<FileMutation>,
        Option<CommandRequest>,
    ),
}

struct Deferred(Option<Prepared>);

impl Drop for Deferred {
    fn drop(&mut self) {
        discard(self.0.take());
    }
}

impl Prepared {
    fn is_parallel(&self) -> bool {
        matches!(self, Self::Ready(_, description, ..) if description.concurrency == Concurrency::Parallel)
    }
}

enum Dispatched {
    Rejected(ToolOutput, ToolRejection),
    Held(ToolOutput),
    Running(JoinHandle<ToolOutput>),
}

struct Settled<'c> {
    call: &'c ToolCall,
    output: Option<ToolOutput>,
    escalates: bool,
}

struct SettledGroup<'c> {
    outcomes: Vec<Settled<'c>>,
    blocked: Option<BlockedCall>,
}

#[derive(Clone, Copy)]
struct Gate<'a> {
    permissions: &'a dyn PermissionGate,
    approvals: Option<&'a Approvals>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Run(PathAccess),
    Held,
    Denied,
    Blocked,
    Interrupted,
}

fn gated_action<'a>(
    call: &'a ToolCall,
    mutation: Option<&'a FileMutation>,
    command: Option<&'a CommandRequest>,
) -> GatedAction<'a> {
    match (mutation, command) {
        (Some(mutation), _) => GatedAction::FileMutation(mutation),
        (None, Some(command)) => GatedAction::Command(command),
        (None, None) => GatedAction::Call(call),
    }
}

fn admit(
    permissions: &dyn PermissionGate,
    action: GatedAction<'_>,
    description: &CallDescription,
) -> Admission {
    match action {
        GatedAction::FileMutation(mutation) => permissions.admit_file_mutation(mutation),
        GatedAction::Command(command) => permissions.admit_command(command),
        GatedAction::Call(_) if description.effect == ToolEffect::None => {
            Admission::Allowed(PathAccess::WorkspaceOnly)
        }
        GatedAction::Call(call) => permissions.admit(call),
    }
}

async fn judge(
    gate: Gate<'_>,
    turn_id: TurnId,
    call: &ToolCall,
    action: GatedAction<'_>,
    description: &CallDescription,
    events: EventSink<'_>,
    cancel: &CancellationToken,
) -> Verdict {
    match admit(gate.permissions, action, description) {
        Admission::Allowed(path_access) => Verdict::Run(path_access),
        Admission::ReviewUnavailable => Verdict::Held,
        Admission::ApprovalRequired => {
            let Some(approvals) = gate.approvals else {
                return Verdict::Blocked;
            };
            let scope = gate.permissions.approval_scope(action);
            let mut pending = approvals.open();
            events(UiEvent::ApprovalRequested {
                turn_id,
                request: approval_request(pending.id(), call, action, description, &scope),
            });
            let answer = tokio::select! {
                biased;
                () = cancel.cancelled() => pending.withdraw(),
                decision = pending.decision() => Some(decision),
            };
            if let (Some(ApprovalDecision::Always), Some(grant)) = (answer, &scope.always) {
                gate.permissions.remember_approval(grant);
            }
            match answer {
                _ if cancel.is_cancelled() => Verdict::Interrupted,
                None | Some(ApprovalDecision::Deny) => Verdict::Denied,
                Some(ApprovalDecision::Once | ApprovalDecision::Always) => {
                    Verdict::Run(scope.access)
                }
            }
        }
    }
}

fn approval_request(
    id: RequestId,
    call: &ToolCall,
    action: GatedAction<'_>,
    description: &CallDescription,
    scope: &ApprovalScope,
) -> ApprovalRequest {
    let (command, file) = match action {
        GatedAction::Call(_) => (None, None),
        GatedAction::FileMutation(mutation) => (None, Some(mutation.clone())),
        GatedAction::Command(command) => (Some(command.clone()), None),
    };
    ApprovalRequest {
        id,
        tool_name: call.name.clone(),
        title: description.title.clone(),
        tool_arguments_preview: encode_terminal_safe(
            call.arguments.as_bytes(),
            MAX_TOOL_ARGUMENTS_PREVIEW_BYTES,
        )
        .text,
        scope: scope.clone(),
        command,
        file,
    }
}

async fn run_group<'c>(
    turn_id: TurnId,
    group: Vec<(&'c ToolCall, Prepared)>,
    gate: Gate<'_>,
    events: EventSink<'_>,
    cancel: &CancellationToken,
) -> SettledGroup<'c> {
    let mut dispatched = Vec::with_capacity(group.len());
    let mut blocked = None;
    let mut group = group.into_iter();
    for (call, prepared) in group.by_ref() {
        if cancel.is_cancelled() {
            discard(prepared);
            break;
        }
        match prepared {
            Prepared::Rejected(Rejection {
                reason,
                title,
                output,
            }) => {
                events(UiEvent::ToolRejected {
                    turn_id,
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    reason,
                    title,
                });
                dispatched.push((call, Dispatched::Rejected(output, reason)));
            }
            Prepared::Ready(prepared, mut description, mutation, command) => {
                let action = gated_action(call, mutation.as_ref(), command.as_ref());
                let verdict = judge(
                    gate,
                    turn_id,
                    call,
                    action,
                    &description,
                    &mut *events,
                    cancel,
                )
                .await;
                if mutation.is_some()
                    && !matches!(verdict, Verdict::Run(_))
                    && let Some(title) = contained(|| prepared.untargeted_title())
                {
                    description.title = title;
                }
                if verdict == Verdict::Blocked {
                    blocked = Some(BlockedCall {
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        title: description.title.clone(),
                    });
                }
                let silent = verdict == Verdict::Interrupted
                    || (mutation.is_some() && verdict == Verdict::Blocked);
                if !silent {
                    events(UiEvent::ToolStarted {
                        turn_id,
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        description,
                    });
                }
                let held = match verdict {
                    Verdict::Run(path_access) => {
                        let context =
                            ToolContext::new(call.id.clone(), cancel.child_token(), path_access);
                        let task = tokio::spawn(async move { prepared.execute(context).await });
                        dispatched.push((call, Dispatched::Running(task)));
                        continue;
                    }
                    Verdict::Held => review_unavailable_json(&call.name),
                    Verdict::Denied => tool_permission_denied_json(&call.name),
                    Verdict::Blocked | Verdict::Interrupted => {
                        discard(prepared);
                        break;
                    }
                };
                discard(prepared);
                dispatched.push((call, Dispatched::Held(ToolOutput::failure(held))));
            }
        }
    }
    group.for_each(discard);
    SettledGroup {
        outcomes: settle_group(turn_id, dispatched, events, cancel).await,
        blocked,
    }
}

async fn settle_group<'c>(
    turn_id: TurnId,
    dispatched: Vec<(&'c ToolCall, Dispatched)>,
    events: EventSink<'_>,
    cancel: &CancellationToken,
) -> Vec<Settled<'c>> {
    let mut grace_deadline = None;
    let mut outcomes = Vec::with_capacity(dispatched.len());
    for (call, dispatched) in dispatched {
        let (output, escalates) = match dispatched {
            Dispatched::Rejected(output, reason) => {
                (Some(output), reason != ToolRejection::MalformedArguments)
            }
            Dispatched::Held(output) => {
                events(tool_finished(turn_id, call, Some(&output)));
                (Some(output), true)
            }
            Dispatched::Running(mut task) => {
                let output = settle(call, &mut task, cancel, &mut grace_deadline).await;
                events(tool_finished(turn_id, call, output.as_ref()));
                (output, true)
            }
        };
        outcomes.push(Settled {
            call,
            output,
            escalates,
        });
    }
    outcomes
}

fn tool_finished(turn_id: TurnId, call: &ToolCall, output: Option<&ToolOutput>) -> UiEvent {
    UiEvent::ToolFinished {
        turn_id,
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        arguments: call.arguments.clone(),
        status: output.map_or(ToolResultStatus::Failure, |output| output.status),
        content: output
            .map(|output| output.content.clone())
            .unwrap_or_default(),
        command_result: output.and_then(|output| output.command_result.clone()),
    }
}

async fn settle(
    call: &ToolCall,
    task: &mut JoinHandle<ToolOutput>,
    cancel: &CancellationToken,
    grace_deadline: &mut Option<Instant>,
) -> Option<ToolOutput> {
    let deadline = if let Some(deadline) = *grace_deadline {
        deadline
    } else {
        tokio::select! {
            biased;
            joined = &mut *task => {
                return Some(settled_output(call, joined));
            }
            () = cancel.cancelled() => {}
        }
        *grace_deadline.insert(Instant::now() + TOOL_CANCEL_GRACE)
    };
    if let Ok(joined) = tokio::time::timeout_at(deadline, &mut *task).await {
        return Some(settled_output(call, joined));
    }
    task.abort();
    None
}

fn settled_output(call: &ToolCall, joined: Result<ToolOutput, JoinError>) -> ToolOutput {
    joined.unwrap_or_else(|error| {
        if let Ok(payload) = error.try_into_panic() {
            release(payload);
        }
        panicked(&call.name)
    })
}

fn contained<T>(hook: impl FnOnce() -> T) -> Option<T> {
    panic::catch_unwind(AssertUnwindSafe(hook))
        .map_err(release)
        .ok()
}

fn release(payload: Box<dyn Any + Send>) {
    if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(move || drop(payload))) {
        mem::forget(payload);
    }
}

fn discard<T>(unused: T) {
    contained(move || drop(unused));
}

fn panicked(tool_name: &str) -> ToolOutput {
    ToolOutput::failure(tool_execution_failure_json(&ExecutionFailure {
        tool_name,
        message: "Tool execution panicked",
        details: &[],
        suggestion: None,
    }))
}

fn escalate_repeated_failure(
    turn: &mut Turn,
    call: &ToolCall,
    status: ToolResultStatus,
    model_output: String,
) -> String {
    if status != ToolResultStatus::Failure {
        return model_output;
    }
    let count = turn
        .failures
        .entry((call.name.clone(), call.arguments.clone()))
        .or_insert(0);
    *count += 1;
    if *count < 2 {
        return model_output;
    }
    format!(
        "{model_output}\n\nThis exact call has already failed {count} times this turn with the same arguments. Do not retry it unchanged."
    )
}

#[cfg(test)]
mod tests;
