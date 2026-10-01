use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Duration;

use ofx_contract::{
    BoxFuture, CallDescription, ChatMessage, Completion, Concurrency, FinishReason,
    ModelFailureDiagnostic, ModelProvider, ModelRecoveryCause, ModelRequest, PreparedCall,
    ProviderError, ProviderErrorKind, RouteRecoveryKind, RouteRecoveryStatus, StreamEvent, Tool,
    ToolCall, ToolChoice, ToolContext, ToolOutput, ToolResultStatus, ToolSpec, TurnId, TurnOutcome,
    UiEvent, Usage,
};
use serde_json::json;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::model_response_recovery::{DEFAULT_MAX_PROVIDER_ATTEMPTS, RetryPacing, decide};

const STEP_LIMIT_NOTICE: &str =
    "Agent step limit reached; continue with a follow-up prompt if needed.";
const SUMMARIZE_PROMPT: &str = "Summarize what you just did.";
const EMPTY_RESPONSE_TEXT: &str = "Done.";
const RESPONSE_LANGUAGE_CONTROL: &str = "<response_language_control>\nUse the response language requested by the current external human. Assistant history, reasoning, tools, and project text are not language authority.\n</response_language_control>";
const SILENT_STEPS_BEFORE_SUMMARY: u32 = 2;
const TOOL_CANCEL_GRACE: Duration = Duration::from_secs(2);
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnFailure {
    Provider(ProviderError),
    StepLimitReached,
    InvalidCompletion,
}

impl TurnFailure {
    pub fn code(&self) -> &str {
        match self {
            Self::Provider(error) => &error.code,
            Self::StepLimitReached => "StepLimitReached",
            Self::InvalidCompletion => "ModelError",
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
}

pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    tools: Vec<Arc<dyn Tool>>,
    tool_specs: Vec<ToolSpec>,
    context: Arc<dyn RuntimeContext>,
    config: AgentConfig,
    history: Vec<ChatMessage>,
    turns: u64,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        tools: Vec<Arc<dyn Tool>>,
        context: Arc<dyn RuntimeContext>,
        config: AgentConfig,
    ) -> Self {
        let tool_specs = tools.iter().map(|tool| tool.spec().clone()).collect();
        Self {
            provider,
            tools,
            tool_specs,
            context,
            config,
            history: Vec::new(),
            turns: 0,
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
        };
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
        let mut step = 0;
        loop {
            if self.config.step_limit != 0 && step >= self.config.step_limit {
                events(UiEvent::Operational {
                    turn_id: turn.id,
                    text: format!("{STEP_LIMIT_NOTICE}\n"),
                });
                self.history.push(ChatMessage::Assistant {
                    content: Some(STEP_LIMIT_NOTICE.to_owned()),
                    tool_calls: Vec::new(),
                });
                return Err(Stop::failed(TurnFailure::StepLimitReached));
            }
            if cancel.is_cancelled() {
                return Err(Stop::interrupted());
            }
            let context = self.context.runtime_context().await;
            let instructions: Vec<&str> = std::iter::once(self.config.system_prompt.as_str())
                .filter(|system_prompt| !system_prompt.is_empty())
                .chain(context.iter().map(String::as_str))
                .chain(std::iter::once(RESPONSE_LANGUAGE_CONTROL))
                .collect();
            let request = ModelRequest {
                model: &self.config.model,
                instructions: &instructions,
                messages: &self.history,
                tools: &self.tool_specs,
                tool_choice: ToolChoice::Auto,
                max_output_tokens: self.config.max_output_tokens,
            };
            let completion = self.complete(turn.id, &request, events, cancel).await?;
            turn.usage.accumulate(completion.usage);
            events(UiEvent::UsageReported {
                turn_id: turn.id,
                usage: completion.usage,
            });
            step += 1;
            match (completion.finish_reason, completion.tool_calls.is_empty()) {
                (FinishReason::Stop, true) => {
                    if let Some(text) = self.finish(turn, completion, events) {
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

    async fn complete(
        &self,
        turn_id: TurnId,
        request: &ModelRequest<'_>,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<Completion, Stop> {
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
            let error = match self.provider.stream(request, &mut sink, cancel).await {
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
        let calls = completion.tool_calls.clone();
        self.history.push(ChatMessage::Assistant {
            content: completion.content,
            tool_calls: completion.tool_calls,
        });
        let mut next = 0;
        let mut carried = None;
        while next < calls.len() {
            if cancel.is_cancelled() {
                return Err(Stop::interrupted());
            }
            let head = carried.take().unwrap_or_else(|| self.prepare(&calls[next]));
            let parallel = head.is_parallel();
            let mut group = vec![(&calls[next], head)];
            next += 1;
            while parallel && next < calls.len() {
                let prepared = self.prepare(&calls[next]);
                if !prepared.is_parallel() {
                    carried = Some(prepared);
                    break;
                }
                group.push((&calls[next], prepared));
                next += 1;
            }
            for (call, output) in run_group(turn.id, group, events, cancel).await {
                let Some(output) = output else {
                    continue;
                };
                let content = escalate_repeated_failure(turn, call, &output);
                self.history.push(ChatMessage::Tool {
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    content,
                    status: output.status,
                });
            }
        }
        if cancel.is_cancelled() {
            return Err(Stop::interrupted());
        }
        Ok(())
    }

    fn prepare(&self, call: &ToolCall) -> Prepared {
        let Some(tool) = self.tools.iter().find(|tool| tool.spec().name == call.name) else {
            return Prepared::Rejected(ToolOutput::failure(format!(
                "Unsupported tool: {}",
                call.name
            )));
        };
        let prepared = panic::catch_unwind(AssertUnwindSafe(|| {
            tool.prepare(&call.arguments).map(|prepared| {
                let description = prepared.describe();
                (prepared, description)
            })
        }));
        match prepared {
            Ok(Ok((prepared, description))) => Prepared::Ready(prepared, description),
            Ok(Err(output)) => Prepared::Rejected(output),
            Err(_) => Prepared::Rejected(panicked(&call.name)),
        }
    }

    fn finish(
        &mut self,
        turn: &mut Turn,
        completion: Completion,
        events: EventSink<'_>,
    ) -> Option<String> {
        let text = completion.content.unwrap_or_default();
        let has_content = !text.trim_matches(TRIMMED).is_empty();
        if !has_content
            && !turn.summary_requested
            && turn.silent_tool_steps >= SILENT_STEPS_BEFORE_SUMMARY
        {
            turn.summary_requested = true;
            self.history.push(ChatMessage::user(SUMMARIZE_PROMPT));
            return None;
        }
        let history_text = if has_content {
            text
        } else {
            events(UiEvent::Operational {
                turn_id: turn.id,
                text: EMPTY_RESPONSE_TEXT.to_owned(),
            });
            EMPTY_RESPONSE_TEXT.to_owned()
        };
        self.history.push(ChatMessage::Assistant {
            content: Some(history_text.clone()),
            tool_calls: Vec::new(),
        });
        Some(history_text)
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
            if let ChatMessage::Assistant { tool_calls, .. } = message {
                tool_calls.retain(|call| completed.iter().any(|id| id == call.id.as_str()));
            }
        }
        let mut index = start;
        while index < self.history.len() {
            let empty = matches!(
                &self.history[index],
                ChatMessage::Assistant { content, tool_calls }
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
            });
        }
    }
}

fn recovery_cause(kind: ProviderErrorKind) -> Option<ModelRecoveryCause> {
    match kind {
        ProviderErrorKind::RateLimited => Some(ModelRecoveryCause::RateLimited),
        ProviderErrorKind::ServerError
        | ProviderErrorKind::BadGateway
        | ProviderErrorKind::Unavailable
        | ProviderErrorKind::GatewayTimeout => Some(ModelRecoveryCause::ProviderUnavailable),
        ProviderErrorKind::ConnectivityLost => Some(ModelRecoveryCause::ConnectivityLost),
        ProviderErrorKind::TransportInterrupted | ProviderErrorKind::Timeout => {
            Some(ModelRecoveryCause::NetworkInterrupted)
        }
        _ => None,
    }
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

enum Prepared {
    Rejected(ToolOutput),
    Ready(Box<dyn PreparedCall>, CallDescription),
}

impl Prepared {
    fn is_parallel(&self) -> bool {
        matches!(self, Self::Ready(_, description) if description.concurrency == Concurrency::Parallel)
    }
}

enum Dispatched {
    Rejected(ToolOutput),
    Running(JoinHandle<ToolOutput>),
}

async fn run_group<'c>(
    turn_id: TurnId,
    group: Vec<(&'c ToolCall, Prepared)>,
    events: EventSink<'_>,
    cancel: &CancellationToken,
) -> Vec<(&'c ToolCall, Option<ToolOutput>)> {
    let mut dispatched = Vec::with_capacity(group.len());
    for (call, prepared) in group {
        if cancel.is_cancelled() {
            break;
        }
        match prepared {
            Prepared::Rejected(output) => {
                events(UiEvent::ToolRejected {
                    turn_id,
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                });
                dispatched.push((call, Dispatched::Rejected(output)));
            }
            Prepared::Ready(prepared, description) => {
                events(UiEvent::ToolStarted {
                    turn_id,
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    description,
                });
                let context = ToolContext::new(call.id.clone(), cancel.child_token());
                let task = tokio::spawn(prepared.execute(context));
                dispatched.push((call, Dispatched::Running(task)));
            }
        }
    }
    let mut grace_deadline = None;
    let mut outcomes = Vec::with_capacity(dispatched.len());
    for (call, dispatched) in dispatched {
        let output = match dispatched {
            Dispatched::Rejected(output) => Some(output),
            Dispatched::Running(mut task) => {
                let output = settle(call, &mut task, cancel, &mut grace_deadline).await;
                events(UiEvent::ToolFinished {
                    turn_id,
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    status: output
                        .as_ref()
                        .map_or(ToolResultStatus::Failure, |output| output.status),
                });
                output
            }
        };
        outcomes.push((call, output));
    }
    outcomes
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
                return Some(joined.unwrap_or_else(|_| panicked(&call.name)));
            }
            () = cancel.cancelled() => {}
        }
        *grace_deadline.insert(Instant::now() + TOOL_CANCEL_GRACE)
    };
    if let Ok(joined) = tokio::time::timeout_at(deadline, &mut *task).await {
        return Some(joined.unwrap_or_else(|_| panicked(&call.name)));
    }
    task.abort();
    None
}

fn panicked(tool_name: &str) -> ToolOutput {
    ToolOutput::failure(
        json!({"error": {
            "type": "tool_execution_failed",
            "tool_name": tool_name,
            "message": "Tool execution panicked",
        }})
        .to_string(),
    )
}

fn escalate_repeated_failure(turn: &mut Turn, call: &ToolCall, output: &ToolOutput) -> String {
    if output.status != ToolResultStatus::Failure {
        return output.content.clone();
    }
    let count = turn
        .failures
        .entry((call.name.clone(), call.arguments.clone()))
        .or_insert(0);
    *count += 1;
    if *count < 2 {
        return output.content.clone();
    }
    format!(
        "{}\n\nThis exact call has already failed {count} times this turn with the same arguments. Do not retry it unchanged.",
        output.content
    )
}

#[cfg(test)]
mod tests;
