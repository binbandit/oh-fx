use std::any::Any;
use std::collections::HashMap;
use std::mem;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Duration;

use ofx_contract::{
    ActiveMode, Admission, ApprovalDecision, ApprovalOrigin, ApprovalRequest, ApprovalScope,
    AutoCompactPercent, BoxFuture, CallDescription, CapabilityLookup, CapabilityResolver,
    ChatMessage, CommandRequest, Completion, Concurrency, ConversationLog,
    DEFAULT_MAX_TOOL_RESULT_BYTES, DynamicTools, ExecutionFailure, FileChange, FileMutation,
    FinishReason, GatedAction, LogFailure, ModelCapabilities, ModelFailureDiagnostic,
    ModelProvider, ModelRecoveryAction, ModelRecoveryCause, ModelRequest, PathAccess,
    PermissionGate, PreparedCall, ProviderError, ProviderErrorKind, ProviderOptions, RecoveredTurn,
    RecoveryStrategy, RequestId, ReviewFailure, ReviewHold, ReviewRequest, ReviewVerdict, Reviewed,
    RootUserRequests, RouteRecoveryKind, RouteRecoveryStatus, SkillBinding, StreamEvent,
    SubagentStatus, SubagentStatusSink, Tool, ToolActivity, ToolArgumentDiagnostic,
    ToolArgumentIntegrity, ToolCall, ToolCallId, ToolContext, ToolEffect, ToolOutput,
    ToolRejection, ToolResultStatus, ToolSpec, TurnId, TurnOutcome, TurnStop, UiEvent, Usage,
    malformed_tool_arguments_json, non_object_tool_arguments_json, prepare_model_output,
    tool_execution_failure_json, tool_permission_denied_json, tool_review_held_json,
};
use ofx_text::encode_terminal_safe;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::approvals::Approvals;
use crate::compactor::{CompactionError, Payload};
use crate::execution_memory::steering_text;
use crate::model_response_recovery::{
    DEFAULT_MAX_PROVIDER_ATTEMPTS, Decision, RetryPacing, decide, recovery_cause,
};
use crate::project_context::{DeliveryState, ProjectContext, ProjectContextProvider};
use crate::prompt_context::Calibration;
use crate::recovery_pause::RecoveryPause;
use crate::skill_context::{SkillContext, SkillContextFailure, SkillContextProvider};
use crate::turn_reviews::TurnReviews;
use crate::worker_runtime::WorkerRuntime;

mod compaction;
mod mode_policy;
mod project_gate;
mod recovery;
mod response_language;
mod steering;
mod turn_ledger;
mod turn_log;

pub use compaction::Compaction;
use compaction::{TurnCompaction, compaction_stop};
use mode_policy::ModePolicy;
use project_gate::GatedGroup;
use recovery::recovery_tool_choice;
use response_language::{Reply, TurnLanguage};
use turn_ledger::TurnLedger;
use turn_log::Ending;

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
    ResponseLanguageMismatch,
    InvalidCompletion,
    PermissionRequired(BlockedCall),
    ProjectContext,
    SkillContext(String),
    Compaction(CompactionError),
    Persistence(LogFailure),
    RecoveryPaused,
}

impl TurnFailure {
    pub fn code(&self) -> &str {
        match self {
            Self::Provider(error) => &error.code,
            Self::StepLimitReached => "StepLimitReached",
            Self::RepeatedMalformedArguments => "RepeatedMalformedToolArguments",
            Self::ResponseLanguageMismatch => "ResponseLanguageMismatch",
            Self::InvalidCompletion => "ModelError",
            Self::PermissionRequired(_) => "NonInteractivePermissionRequired",
            Self::ProjectContext => "ProjectContextFailed",
            Self::SkillContext(code) => code,
            Self::Compaction(error) => error.code(),
            Self::Persistence(failure) => &failure.code,
            Self::RecoveryPaused => "RecoveryPaused",
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
    compaction: TurnCompaction,
    raw_outputs: Vec<(ToolCallId, usize)>,
    reviews: TurnReviews,
    language: TurnLanguage,
    recovery: Option<RecoveryStrategy>,
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

struct LastReply {
    turn: usize,
    text: Arc<str>,
}

fn describe_tools(tools: &[Arc<dyn Tool>]) -> (Vec<ToolSpec>, Vec<ToolSpec>, String) {
    let tool_specs: Vec<ToolSpec> = tools.iter().map(|tool| tool.spec().clone()).collect();
    let (remote, offered): (Vec<_>, Vec<_>) = tools
        .iter()
        .zip(&tool_specs)
        .partition(|(tool, _)| tool.provider_executed());
    let offered_specs = offered.into_iter().map(|(_, spec)| spec.clone()).collect();
    let tool_guidance = remote
        .iter()
        .map(|(_, spec)| spec.description.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    (tool_specs, offered_specs, tool_guidance)
}

struct DynamicToolSet {
    source: Arc<dyn DynamicTools>,
    generation: Option<u64>,
    tools: Vec<Arc<dyn Tool>>,
}

pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    tools: Vec<Arc<dyn Tool>>,
    tool_specs: Vec<ToolSpec>,
    offered_specs: Vec<ToolSpec>,
    tool_guidance: String,
    dynamic: Option<DynamicToolSet>,
    mode: Option<ModePolicy>,
    context: Arc<dyn RuntimeContext>,
    permissions: Arc<dyn PermissionGate>,
    approvals: Option<Approvals>,
    config: AgentConfig,
    capability_resolver: Option<Arc<dyn CapabilityResolver>>,
    capabilities: Option<KnownCapabilities>,
    project: Option<ProjectInstructions>,
    skills: Option<Arc<dyn SkillContextProvider>>,
    history: Vec<ChatMessage>,
    turn_starts: Vec<usize>,
    inherited_requests: Option<Arc<RootUserRequests>>,
    ledger: TurnLedger,
    compacted: Option<Payload>,
    calibration: Option<Calibration>,
    session_id: Option<String>,
    log: Option<Box<dyn ConversationLog>>,
    request_fixed_tokens: Option<usize>,
    turns: u64,
    last_reply: Option<LastReply>,
    steering: Option<Arc<WorkerRuntime>>,
    recovery_pause: RecoveryPause,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        tools: Vec<Arc<dyn Tool>>,
        context: Arc<dyn RuntimeContext>,
        permissions: Arc<dyn PermissionGate>,
        config: AgentConfig,
    ) -> Self {
        let (tool_specs, offered_specs, tool_guidance) = describe_tools(&tools);
        Self {
            provider,
            tools,
            tool_specs,
            offered_specs,
            tool_guidance,
            dynamic: None,
            mode: None,
            context,
            permissions,
            approvals: None,
            config,
            capability_resolver: None,
            capabilities: None,
            project: None,
            skills: None,
            history: Vec::new(),
            turn_starts: Vec::new(),
            inherited_requests: None,
            ledger: TurnLedger::default(),
            compacted: None,
            calibration: None,
            session_id: None,
            log: None,
            request_fixed_tokens: None,
            turns: 0,
            last_reply: None,
            steering: None,
            recovery_pause: RecoveryPause::default(),
        }
    }

    #[must_use]
    pub fn with_mode(mut self, mode: ActiveMode) -> Self {
        self.mode = Some(ModePolicy::new(mode, &self.tools));
        self
    }

    #[must_use]
    pub fn with_approvals(mut self, approvals: Approvals) -> Self {
        self.approvals = Some(approvals);
        self
    }

    #[must_use]
    pub fn with_dynamic_tools(mut self, source: Arc<dyn DynamicTools>) -> Self {
        self.dynamic = Some(DynamicToolSet {
            source,
            generation: None,
            tools: Vec::new(),
        });
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

    #[must_use]
    pub fn with_skills(mut self, skills: Arc<dyn SkillContextProvider>) -> Self {
        self.skills = Some(skills);
        self
    }

    pub(crate) fn config(&self) -> &AgentConfig {
        &self.config
    }

    pub(crate) fn replace_tools(&mut self, tools: Vec<Arc<dyn Tool>>) {
        (self.tool_specs, self.offered_specs, self.tool_guidance) = describe_tools(&tools);
        self.tools = tools;
        if let Some(set) = &mut self.dynamic {
            set.generation = None;
        }
    }

    pub(crate) fn inherit_root_user_requests(&mut self, requests: Arc<RootUserRequests>) {
        self.inherited_requests = Some(requests);
    }

    pub fn set_config(&mut self, config: AgentConfig) {
        if config.model != self.config.model
            || self
                .capabilities
                .as_ref()
                .is_some_and(|known| known.catalog_unavailable)
        {
            self.capabilities = None;
        }
        self.config = config;
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        self.turn_starts.clear();
        self.ledger.reset(0);
        self.compacted = None;
        self.calibration = None;
        self.last_reply = None;
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
        self.run_turn_with_skills(prompt, &[], events, cancel).await
    }

    pub fn run_turn_with_skills<'a>(
        &'a mut self,
        prompt: &'a str,
        skills: &'a [SkillBinding],
        events: EventSink<'a>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, TurnReport> {
        Box::pin(self.turn(prompt, skills, events, cancel))
    }

    async fn turn(
        &mut self,
        prompt: &str,
        skills: &[SkillBinding],
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> TurnReport {
        self.run_prompt(prompt, skills, None, events, cancel).await
    }

    pub async fn continue_turn(
        &mut self,
        recovered: RecoveredTurn,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> TurnReport {
        let prompt = recovered.prompt.clone();
        self.run_prompt(&prompt, &[], Some(recovered), events, cancel)
            .await
    }

    async fn run_prompt(
        &mut self,
        prompt: &str,
        skills: &[SkillBinding],
        recovered: Option<RecoveredTurn>,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> TurnReport {
        self.turns += 1;
        self.recovery_pause.reset();
        let id = TurnId::new(self.turns);
        events(UiEvent::TurnStarted { turn_id: id });
        if let Err(failure) = self.require_writable() {
            events(UiEvent::TurnFinished {
                turn_id: id,
                outcome: TurnOutcome::Failed,
            });
            return TurnReport {
                outcome: TurnOutcome::Failed,
                final_text: String::new(),
                usage: Usage::default(),
                failure: Some(TurnFailure::Persistence(failure)),
            };
        }
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
            compaction: TurnCompaction::default(),
            raw_outputs: Vec::new(),
            reviews: TurnReviews::default(),
            language: self.turn_language(prompt),
            recovery: None,
        };
        self.turn_starts.push(turn.start);
        self.history.push(self.turn_message(prompt));
        if let Some(recovered) = recovered {
            self.history.extend(recovered.messages);
            turn.fast_mode = recovered.fast_mode;
            turn.recovery = Some(recovered.strategy);
        }
        let result = self.drive(&mut turn, prompt, skills, events, cancel).await;
        let (outcome, final_text, mut failure, ending) = match result {
            Ok(text) => (TurnOutcome::Completed, text, None, Ending::Replied),
            Err(Stop::Interrupted { partial }) => {
                self.keep_partial_turn(turn.start, &partial);
                (
                    TurnOutcome::Interrupted,
                    String::new(),
                    None,
                    Ending::Stopped(TurnStop::Cancelled),
                )
            }
            Err(Stop::Failed { failure, partial }) => {
                let spoke = !partial.trim_matches(TRIMMED).is_empty();
                let ending = if failure == TurnFailure::RecoveryPaused && self.log.is_some() {
                    self.keep_partial_turn(turn.start, &partial);
                    Ending::Stopped(TurnStop::Failed)
                } else if !spoke
                    && !self.has_turn_progress(turn.start)
                    && !turn.compaction.compacted_steps
                    && failure != TurnFailure::StepLimitReached
                {
                    self.history.truncate(turn.start);
                    self.turn_starts.pop();
                    Ending::Discarded
                } else if spoke {
                    self.keep_partial_turn(turn.start, &partial);
                    Ending::Stopped(TurnStop::Failed)
                } else {
                    self.keep_partial_turn(turn.start, "");
                    Ending::Replied
                };
                (TurnOutcome::Failed, String::new(), Some(failure), ending)
            }
        };
        let recorded = self.record_turn(prompt, &turn, ending);
        self.settle_steering(turn.start);
        self.note_recorded(&turn, ending, recorded.is_ok());
        if let Err(error) = recorded
            && failure.is_none()
        {
            failure = Some(TurnFailure::Persistence(error));
        }
        events(UiEvent::TurnFinished {
            turn_id: id,
            outcome,
        });
        if outcome == TurnOutcome::Completed {
            self.last_reply = Some(LastReply {
                turn: self.turn_starts.len().saturating_sub(1),
                text: Arc::from(final_text.as_str()),
            });
        }
        TurnReport {
            outcome,
            final_text,
            usage: turn.usage,
            failure,
        }
    }

    pub fn history_turns(&self) -> usize {
        self.turn_starts.len() + usize::from(self.compacted.is_some())
    }

    pub fn last_assistant_reply(&self) -> Option<Arc<str>> {
        self.last_reply
            .as_ref()
            .map(|reply| Arc::clone(&reply.text))
    }

    fn known_context_window(&self) -> Option<u32> {
        self.capabilities
            .as_ref()
            .and_then(|known| known.model.context_window)
    }

    async fn drive(
        &mut self,
        turn: &mut Turn,
        prompt: &str,
        bindings: &[SkillBinding],
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<String, Stop> {
        if self.config.reasoning_effort.is_some() || self.config.fast_mode {
            self.resolve_capabilities(cancel).await?;
        }
        let skills = self
            .prepare_skills(turn.id, prompt, bindings, events, cancel)
            .await?;
        let mut step = 0;
        loop {
            self.stop_at_step_limit(turn.id, step, events)?;
            let step_cancel = self.begin_model_step(turn, events, cancel)?;
            if self.has_compactable_context(turn) {
                self.resolve_capabilities(cancel).await?;
            }
            self.refresh_dynamic_tools(turn.id, events);
            let context = self.context.runtime_context().await;
            let instructions = self.instructions(&skills, &context);
            let messages = self.request_messages(turn);
            let request = self.turn_request(turn, &instructions, &messages, events);
            let (measured, body) = self.measure(turn, &request).unzip();
            match self
                .preflight(turn, request, measured.as_ref(), events, cancel)
                .await
            {
                Ok(None) => {}
                Ok(Some(compacted)) => {
                    self.adopt_compaction(turn, compacted, measured, events)?;
                    continue;
                }
                Err(error) => return Err(compaction_stop(error, cancel)),
            }
            self.begin_language_request(turn, &instructions);
            let outcome = self
                .complete(turn, request, body, events, &step_cancel)
                .await
                .map_err(|stop| turn.language.filter_stop(stop));
            let completion = match outcome {
                Ok(completion) => {
                    self.settle_measurement(measured, completion.usage.input_tokens);
                    turn.recovery = None;
                    completion
                }
                Err(Stop::Failed {
                    failure: TurnFailure::Provider(error),
                    partial,
                }) if self.recovers_overflow(turn, &error, &partial, cancel) => {
                    self.settle_measurement(measured, None);
                    continue;
                }
                Err(Stop::Interrupted { partial }) if self.steers_after_interrupt(cancel) => {
                    self.settle_measurement(measured, None);
                    self.keep_interrupted_reply(&partial);
                    step += 1;
                    continue;
                }
                Err(ended) => {
                    self.settle_measurement(measured, None);
                    return Err(ended);
                }
            };
            let completion =
                match self.settle_reply(turn, completion, &step_cancel, cancel, events)? {
                    Reply::Accepted(completion) => completion,
                    Reply::Steered => {
                        step += 1;
                        continue;
                    }
                    Reply::Rejected => continue,
                };
            step += 1;
            let more_steps = self.config.step_limit == 0 || step < self.config.step_limit;
            match (completion.finish_reason, completion.tool_calls.is_empty()) {
                (FinishReason::Stop, true) => {
                    if let Some(text) = self.finish(turn, completion, more_steps, events)? {
                        return Ok(text);
                    }
                }
                (FinishReason::ToolCalls, false) => {
                    self.run_batch(turn, completion, more_steps, events, cancel)
                        .await?;
                }
                _ => return Err(Stop::failed(TurnFailure::InvalidCompletion)),
            }
        }
    }

    fn instructions<'a>(&'a self, skills: &'a SkillContext, context: &'a [String]) -> Vec<&'a str> {
        let deltas = self
            .project
            .as_ref()
            .map_or(0, |project| project.deltas.len());
        let mut instructions: Vec<&str> = Vec::with_capacity(context.len() + deltas + 6);
        if !self.config.system_prompt.is_empty() {
            instructions.push(&self.config.system_prompt);
        }
        if !self.tool_guidance.is_empty() {
            instructions.push(&self.tool_guidance);
        }
        if !skills.catalog.is_empty() {
            instructions.push(&skills.catalog);
        }
        if let Some(project) = &self.project {
            instructions.extend(project.snapshot.as_deref());
            instructions.extend(project.deltas.iter().map(String::as_str));
        }
        if !skills.explicit.is_empty() {
            instructions.push(&skills.explicit);
        }
        instructions.extend(context.iter().map(String::as_str));
        if self.answers_the_root_user() {
            instructions.push(RESPONSE_LANGUAGE_CONTROL);
        }
        instructions
    }

    async fn prepare_skills(
        &mut self,
        turn_id: TurnId,
        prompt: &str,
        bindings: &[SkillBinding],
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<SkillContext, Stop> {
        let Some(skills) = self.skills.clone() else {
            return Ok(SkillContext::default());
        };
        if skills.uses_context_window() {
            self.resolve_capabilities(cancel).await?;
        }
        let context_window = self.known_context_window();
        let mut report = |notices: Vec<String>| {
            for text in notices {
                events(UiEvent::ContextNotice { turn_id, text });
            }
        };
        let mut prepared = match skills
            .prepare(prompt, bindings, context_window, cancel)
            .await
        {
            Ok(prepared) => prepared,
            Err(SkillContextFailure::Cancelled) => return Err(Stop::interrupted()),
            Err(SkillContextFailure::Failed {
                code,
                context_notices,
            }) => {
                report(context_notices);
                return Err(Stop::failed(TurnFailure::SkillContext(code)));
            }
        };
        report(mem::take(&mut prepared.context_notices));
        if let Some(notice) = prepared.load_notice.take() {
            events(UiEvent::Notice { notice });
        }
        Ok(prepared)
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

    fn turn_request<'a>(
        &'a self,
        turn: &mut Turn,
        instructions: &'a [&'a str],
        messages: &'a [ChatMessage],
        events: EventSink<'_>,
    ) -> ModelRequest<'a> {
        ModelRequest {
            model: &self.config.model,
            instructions,
            messages,
            tools: self.advertised_tools(),
            tool_choice: recovery_tool_choice(turn.recovery),
            max_output_tokens: self.config.max_output_tokens,
            provider_options: self.provider_options(turn, events),
            session_id: self.session_id.as_deref(),
        }
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
        mut body: Option<String>,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<Completion, Stop> {
        let turn_id = turn.id;
        let mut attempt = 1;
        let mut pacing = RetryPacing::Idle;
        let mut recovering_from = None;
        let mut pending = None;
        loop {
            let mut partial = String::new();
            let mut admitted = false;
            let mut sink = |event: StreamEvent| match event {
                StreamEvent::Admitted => {
                    admitted = true;
                    if let Some(status) = pending.take() {
                        events(UiEvent::Recovery { turn_id, status });
                    }
                }
                StreamEvent::TextDelta { text } => {
                    partial.push_str(&text);
                    if let Some(text) = turn.language.stage.admit(text) {
                        events(UiEvent::AssistantText { turn_id, text });
                    }
                }
                StreamEvent::ReasoningDelta { text } => {
                    events(UiEvent::ReasoningText { turn_id, text });
                }
            };
            let streamed = match body.take() {
                Some(body) => {
                    self.provider
                        .stream_body(&request, body, &mut sink, cancel)
                        .await
                }
                None => self.provider.stream(&request, &mut sink, cancel).await,
            };
            let consumed = attempt - usize::from(!admitted);
            let error = match streamed {
                Ok(completion) => {
                    if recovering_from.is_some() {
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
                let recovery = recovering_from.map(|cause| (cause, consumed));
                return Err(self.interruption(turn_id, recovery, &error, partial, events));
            }
            let cause = recovery_cause(error.kind).filter(|_| partial.is_empty());
            let Some(cause) = cause.filter(|_| attempt < DEFAULT_MAX_PROVIDER_ATTEMPTS) else {
                if let Some(status) = stopped_status(
                    cause.or(recovering_from),
                    attempt,
                    consumed,
                    &error,
                    &partial,
                ) {
                    events(UiEvent::Recovery { turn_id, status });
                }
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
            let mut status = retry_status(attempt, cause, &decision, &error);
            events(UiEvent::Recovery {
                turn_id,
                status: status.clone(),
            });
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    let recovery = Some((cause, consumed));
                    return Err(self.interruption(turn_id, recovery, &error, String::new(), events));
                }
                () = tokio::time::sleep(decision.delay) => {}
            }
            attempt += 1;
            status.failed_attempt = attempt;
            status.retry_wait = None;
            pending = Some(status);
            pacing = decision.next_pacing;
            recovering_from = Some(cause);
        }
    }

    pub fn recovery_pause(&self) -> RecoveryPause {
        self.recovery_pause.clone()
    }

    fn interruption(
        &self,
        turn_id: TurnId,
        recovery: Option<(ModelRecoveryCause, usize)>,
        error: &ProviderError,
        partial: String,
        events: EventSink<'_>,
    ) -> Stop {
        let Some((cause, attempt)) = recovery.filter(|_| self.recovery_pause.requested()) else {
            return Stop::Interrupted { partial };
        };
        events(UiEvent::Recovery {
            turn_id,
            status: RouteRecoveryStatus {
                kind: RouteRecoveryKind::TerminalProviderError,
                failed_attempt: attempt,
                succeeded_attempt: 0,
                attempt_limit: DEFAULT_MAX_PROVIDER_ATTEMPTS,
                cause: Some(cause),
                action: Some(ModelRecoveryAction::Paused),
                delay_seconds: 0,
                diagnostic: Some(failure_diagnostic(error)),
                retry_wait: None,
            },
        });
        Stop::Failed {
            failure: TurnFailure::RecoveryPaused,
            partial,
        }
    }

    async fn run_batch(
        &mut self,
        turn: &mut Turn,
        completion: Completion,
        more_steps: bool,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), Stop> {
        self.enter_tool_phase();
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
                        turn.raw_outputs
                            .push((calls[next].id.clone(), output.len()));
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
            let mut reviewing = Reviewing {
                model: &self.config.model,
                history: &self.history,
                turn_starts: &self.turn_starts,
                compacted_turns: self.compacted.as_ref().map(|payload| payload.turn_count),
                inherited_requests: self.inherited_requests.as_ref(),
                turn_start: turn.start,
                batch: &calls,
                reviews: &mut turn.reviews,
                usage: &mut turn.usage,
            };
            let settled = run_group(turn.id, group, gate, &mut reviewing, events, cancel).await;
            self.record_settled(turn, settled.outcomes);
            if let Some(blocked) = settled.blocked {
                return Err(Stop::failed(TurnFailure::PermissionRequired(blocked)));
            }
        }
        if cancel.is_cancelled() {
            return Err(Stop::interrupted());
        }
        turn.malformed_batches = if all_malformed {
            (turn.malformed_batches + 1).min(MAX_CONSECUTIVE_MALFORMED_ARGUMENT_BATCHES)
        } else {
            0
        };
        if turn.malformed_batches == MAX_CONSECUTIVE_MALFORMED_ARGUMENT_BATCHES {
            if more_steps && let Some(steering) = self.finalizing_steering() {
                self.append_steering(turn.id, steering, events);
                return Ok(());
            }
            return Err(self.stop_with_notice(
                turn.id,
                events,
                REPEATED_MALFORMED_ARGUMENTS_NOTICE,
                TurnFailure::RepeatedMalformedArguments,
            ));
        }
        Ok(())
    }

    fn record_settled(&mut self, turn: &mut Turn, outcomes: Vec<Settled<'_>>) {
        for Settled {
            call,
            output,
            escalates,
            review_hold,
        } in outcomes
        {
            let Some(output) = output else {
                continue;
            };
            let status = output.status;
            turn.raw_outputs
                .push((call.id.clone(), output.content.len()));
            let model_output =
                prepare_model_output(&call.name, output.content, DEFAULT_MAX_TOOL_RESULT_BYTES);
            let content = if escalates {
                escalate_repeated_failure(turn, call, status, model_output)
            } else {
                model_output
            };
            if review_hold {
                turn.reviews.record_held_result(&call.id, &content);
            }
            self.history.push(ChatMessage::Tool {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                content,
                status,
            });
        }
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

    fn advertised_tools(&self) -> &[ToolSpec] {
        self.mode
            .as_ref()
            .map_or(&self.offered_specs, ModePolicy::advertised)
    }

    fn tool(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        let dynamic = self.dynamic.iter().flat_map(|set| &set.tools);
        self.tools
            .iter()
            .chain(dynamic)
            .zip(&self.tool_specs)
            .find_map(|(tool, spec)| (spec.name == name).then_some(tool))
    }

    fn stop_at_step_limit(
        &mut self,
        turn_id: TurnId,
        step: u64,
        events: EventSink<'_>,
    ) -> Result<(), Stop> {
        if self.config.step_limit != 0 && step >= self.config.step_limit {
            return Err(self.stop_with_notice(
                turn_id,
                events,
                STEP_LIMIT_NOTICE,
                TurnFailure::StepLimitReached,
            ));
        }
        Ok(())
    }

    fn refresh_dynamic_tools(&mut self, turn_id: TurnId, events: EventSink<'_>) {
        let Some(set) = &mut self.dynamic else {
            return;
        };
        let generation = set.source.generation();
        if set.generation == Some(generation) {
            return;
        }
        set.generation = Some(generation);
        set.tools = set.source.tools();
        for text in set.source.take_notices() {
            events(UiEvent::ContextNotice { turn_id, text });
        }
        self.tool_specs.truncate(self.tools.len());
        self.tool_specs
            .extend(set.tools.iter().map(|tool| tool.spec().clone()));
        let offered = self.tools.iter().filter(|tool| !tool.provider_executed());
        self.offered_specs.truncate(offered.count());
        self.offered_specs.extend(
            set.tools
                .iter()
                .filter(|tool| !tool.provider_executed())
                .map(|tool| tool.spec().clone()),
        );
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
        let head = match carried.0.take() {
            Some(uncompleted) => uncompleted.complete(&calls[start].name),
            None => self.prepare(&calls[start], malformed[start].take()),
        };
        let parallel = head.parallel_group();
        let mut group = vec![(&calls[start], head)];
        for (call, malformed) in calls[start + 1..].iter().zip(&mut malformed[start + 1..]) {
            if parallel.is_none() {
                break;
            }
            let uncompleted = self.prepare_uncompleted(call, malformed.take());
            if uncompleted.parallel_group() != parallel {
                carried.0 = Some(uncompleted);
                break;
            }
            group.push((call, uncompleted.complete(&call.name)));
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
                description: None,
                output,
            });
        }
        if let Some(output) = self
            .mode
            .as_ref()
            .and_then(|mode| mode.denial(&self.tools, &call.name))
        {
            return Err(Rejection {
                reason: ToolRejection::Invalid,
                description: None,
                output,
            });
        }
        let Some(tool) = self.tool(&call.name) else {
            return Err(Rejection {
                reason: ToolRejection::Unsupported,
                description: None,
                output: ToolOutput::failure(format!("Unsupported tool: {}", call.name)),
            });
        };
        match contained(|| tool.prepare(&call.arguments)) {
            Some(Ok(prepared)) => Ok(prepared),
            Some(Err(output)) => Err(Rejection {
                reason: ToolRejection::Invalid,
                description: None,
                output,
            }),
            None => Err(Rejection::panicked(&call.name)),
        }
    }

    fn finish(
        &mut self,
        turn: &mut Turn,
        completion: Completion,
        more_steps: bool,
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
            (EMPTY_RESPONSE_TEXT.to_owned(), replay)
        };
        let reply = ChatMessage::Assistant {
            content: Some(history_text.clone()),
            tool_calls: Vec::new(),
            provider_replay: history_replay,
        };
        if more_steps && let Some(steering) = self.finalizing_steering() {
            self.history.push(reply);
            self.append_steering(turn.id, steering, events);
            return Ok(None);
        }
        if !has_content {
            events(UiEvent::Operational {
                turn_id: turn.id,
                text: EMPTY_RESPONSE_TEXT.to_owned(),
            });
        }
        self.history.push(reply);
        Ok(Some(history_text))
    }

    fn has_turn_progress(&self, start: usize) -> bool {
        self.history[start + 1..]
            .iter()
            .any(|message| match message {
                ChatMessage::Tool { .. } => true,
                ChatMessage::User {
                    content,
                    restored_steering,
                } => *restored_steering || steering_text(content).is_some(),
                ChatMessage::System { .. } | ChatMessage::Assistant { .. } => false,
            })
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

fn retry_status(
    attempt: usize,
    cause: ModelRecoveryCause,
    decision: &Decision,
    error: &ProviderError,
) -> RouteRecoveryStatus {
    RouteRecoveryStatus {
        kind: RouteRecoveryKind::AutoRetry,
        failed_attempt: attempt,
        succeeded_attempt: 0,
        attempt_limit: DEFAULT_MAX_PROVIDER_ATTEMPTS,
        cause: Some(cause),
        action: Some(decision.action),
        delay_seconds: decision.delay.as_secs(),
        diagnostic: Some(failure_diagnostic(error)),
        retry_wait: Some(decision.delay),
    }
}

fn stopped_status(
    cause: Option<ModelRecoveryCause>,
    attempt: usize,
    consumed: usize,
    error: &ProviderError,
    partial: &str,
) -> Option<RouteRecoveryStatus> {
    let exhausted =
        attempt >= DEFAULT_MAX_PROVIDER_ATTEMPTS && recovery_cause(error.kind).is_some();
    let unanswered = attempt > 1 && error.status.is_none();
    (partial.is_empty() && (exhausted || unanswered)).then(|| RouteRecoveryStatus {
        kind: RouteRecoveryKind::TerminalProviderError,
        failed_attempt: consumed,
        succeeded_attempt: 0,
        attempt_limit: DEFAULT_MAX_PROVIDER_ATTEMPTS,
        cause,
        action: None,
        delay_seconds: 0,
        diagnostic: Some(failure_diagnostic(error)),
        retry_wait: None,
    })
}

fn failure_diagnostic(error: &ProviderError) -> ModelFailureDiagnostic {
    ModelFailureDiagnostic::new(error.diagnostic.as_deref().unwrap_or(&error.code))
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
        retry_wait: None,
    }
}

struct Rejection {
    reason: ToolRejection,
    description: Option<Box<CallDescription>>,
    output: ToolOutput,
}

impl Rejection {
    fn panicked(tool_name: &str) -> Self {
        Self {
            reason: ToolRejection::Panicked,
            description: None,
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
            description: Some(Box::new(description)),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParallelGroup {
    ReadOnly,
    Subagent,
}

fn parallel_group(description: &CallDescription) -> Option<ParallelGroup> {
    match (description.concurrency, description.activity) {
        (Concurrency::Serial, _) => None,
        (Concurrency::Parallel, ToolActivity::Subagent) => Some(ParallelGroup::Subagent),
        (Concurrency::Parallel, _) => Some(ParallelGroup::ReadOnly),
    }
}

impl Prepared {
    fn parallel_group(&self) -> Option<ParallelGroup> {
        match self {
            Self::Ready(_, description, ..) => parallel_group(description),
            Self::Rejected(_) => None,
        }
    }

    fn complete(self, tool_name: &str) -> Self {
        match self {
            Self::Ready(prepared, ..) => completed(prepared, tool_name),
            rejected @ Self::Rejected(_) => rejected,
        }
    }
}

enum Dispatched {
    Rejected(ToolOutput, ToolRejection),
    Held(ToolOutput, bool),
    Running(JoinHandle<ToolOutput>),
}

struct Settled<'c> {
    call: &'c ToolCall,
    output: Option<ToolOutput>,
    escalates: bool,
    review_hold: bool,
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

struct Reviewing<'a> {
    model: &'a str,
    history: &'a [ChatMessage],
    turn_starts: &'a [usize],
    compacted_turns: Option<usize>,
    inherited_requests: Option<&'a Arc<RootUserRequests>>,
    turn_start: usize,
    batch: &'a [ToolCall],
    reviews: &'a mut TurnReviews,
    usage: &'a mut Usage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Run(PathAccess),
    Held(String),
    Denied,
    Blocked,
    Interrupted,
}

fn gated_action<'a>(
    call: &'a ToolCall,
    mutation: Option<&'a FileMutation>,
    command: Option<&'a CommandRequest>,
    prepared: &dyn PreparedCall,
) -> GatedAction<'a> {
    match (mutation, command) {
        (Some(mutation), _) => GatedAction::FileMutation(mutation),
        (None, Some(command)) => GatedAction::Command(command),
        (None, None) if contained(|| prepared.mcp_tool()) == Some(true) => {
            GatedAction::McpTool(call)
        }
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
        GatedAction::McpTool(call) => permissions.admit_mcp_tool(call),
        GatedAction::Call(_) if description.effect == ToolEffect::None => {
            Admission::Allowed(PathAccess::WorkspaceOnly)
        }
        GatedAction::Call(call) => permissions.admit(call),
    }
}

struct Judged<'a> {
    call: &'a ToolCall,
    action: GatedAction<'a>,
    description: &'a CallDescription,
    evidence: &'a ReviewEvidence<'a>,
}

#[derive(Default)]
struct ReviewEvidence<'p> {
    file: Option<FileChange<'p>>,
    schema: Option<String>,
}

fn admission<'p>(
    gate: Gate<'_>,
    action: GatedAction<'_>,
    description: &CallDescription,
    prepared: &'p dyn PreparedCall,
) -> (Admission, ReviewEvidence<'p>) {
    let admission = admit(gate.permissions, action, description);
    let shown = match admission {
        Admission::ReviewRequired => true,
        Admission::ApprovalRequired => gate.approvals.is_some(),
        Admission::Allowed(_) => false,
    };
    let evidence = match action {
        GatedAction::FileMutation(_) if shown => ReviewEvidence {
            file: contained(|| prepared.file_change()).flatten(),
            schema: None,
        },
        GatedAction::McpTool(_) if matches!(admission, Admission::ReviewRequired) => {
            ReviewEvidence {
                file: None,
                schema: contained(|| prepared.review_schema()).flatten(),
            }
        }
        _ => ReviewEvidence::default(),
    };
    (admission, evidence)
}

async fn judge(
    gate: Gate<'_>,
    reviewing: &mut Reviewing<'_>,
    turn_id: TurnId,
    admission: Admission,
    judged: Judged<'_>,
    events: EventSink<'_>,
    cancel: &CancellationToken,
) -> Verdict {
    match admission {
        Admission::Allowed(path_access) => Verdict::Run(path_access),
        Admission::ApprovalRequired => ask_approval(gate, turn_id, &judged, events, cancel).await,
        Admission::ReviewRequired => {
            let Some(verdict) = review(gate, reviewing, &judged, cancel).await else {
                return Verdict::Interrupted;
            };
            let call = judged.call;
            match verdict {
                ReviewVerdict::Clear => {
                    Verdict::Run(gate.permissions.approval_scope(judged.action).access)
                }
                ReviewVerdict::Caution(advice) => Verdict::Held(tool_review_held_json(
                    &call.name,
                    ReviewHold::Caution(&advice),
                )),
                _ if gate.approvals.is_some() => {
                    ask_approval(gate, turn_id, &judged, events, cancel).await
                }
                ReviewVerdict::EvidenceIncomplete => Verdict::Held(tool_review_held_json(
                    &call.name,
                    ReviewHold::EvidenceIncomplete,
                )),
                ReviewVerdict::Unavailable(failure) => Verdict::Held(tool_review_held_json(
                    &call.name,
                    ReviewHold::Unavailable(failure),
                )),
            }
        }
    }
}

async fn review(
    gate: Gate<'_>,
    reviewing: &mut Reviewing<'_>,
    judged: &Judged<'_>,
    cancel: &CancellationToken,
) -> Option<ReviewVerdict> {
    let call = judged.call;
    if let Some(verdict) = reviewing.reviews.cached(call) {
        return Some(verdict);
    }
    let attempt_available = reviewing.reviews.attempt_available(call);
    let (current_request, earlier_requests, compacted_turns) = reviewing.root_user_requests();
    let request = ReviewRequest {
        model: reviewing.model,
        current_request,
        earlier_requests: &earlier_requests,
        compacted_turns,
        turn: &reviewing.history[reviewing.turn_start..],
        held: reviewing.reviews.held_results(),
        batch: reviewing.batch,
        call,
        action: judged.action,
        file: judged.evidence.file.as_ref(),
        schema: judged.evidence.schema.as_deref(),
        attempt_available,
    };
    let reviewed = tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        reviewed = gate.permissions.review(request, cancel) => reviewed,
    };
    if cancel.is_cancelled() {
        return None;
    }
    let reviewed =
        reviewed.unwrap_or_else(|| Reviewed::unavailable(ReviewFailure::TransportTransient));
    reviewing.usage.accumulate(reviewed.usage);
    reviewing.reviews.remember(call, &reviewed.verdict);
    Some(reviewed.verdict)
}

impl<'a> Reviewing<'a> {
    fn root_user_requests(&self) -> (&'a str, Vec<&'a str>, Option<usize>) {
        if let Some(inherited) = self.inherited_requests {
            return (
                &inherited.current,
                inherited.earlier.iter().map(String::as_str).collect(),
                inherited.compacted_turns,
            );
        }
        let (current, earlier) = root_requests(self.history, self.turn_starts);
        (current, earlier, self.compacted_turns)
    }

    fn delegated_requests(&self) -> Arc<RootUserRequests> {
        if let Some(inherited) = self.inherited_requests {
            return Arc::clone(inherited);
        }
        let (current, earlier, compacted_turns) = self.root_user_requests();
        Arc::new(RootUserRequests {
            current: current.to_owned(),
            earlier: earlier.into_iter().map(str::to_owned).collect(),
            compacted_turns,
        })
    }

    fn tool_context(
        &self,
        turn_id: TurnId,
        call: &ToolCall,
        delegation: Option<&UnboundedSender<ChildStatus>>,
        path_access: PathAccess,
        cancel: &CancellationToken,
    ) -> ToolContext {
        let context =
            ToolContext::new(call.id.clone(), cancel.child_token(), path_access).with_turn(turn_id);
        let Some(statuses) = delegation else {
            return context;
        };
        let statuses = statuses.clone();
        let call_id = call.id.clone();
        context
            .with_root_user_requests(self.delegated_requests())
            .with_subagent_status(SubagentStatusSink::new(move |status| {
                let _ = statuses.send((call_id.clone(), status));
            }))
    }
}

fn root_requests<'h>(history: &'h [ChatMessage], turn_starts: &[usize]) -> (&'h str, Vec<&'h str>) {
    let user_request = |start: &usize| match history.get(*start) {
        Some(ChatMessage::User { content, .. }) => Some(steering_text(content).unwrap_or(content)),
        _ => None,
    };
    let Some((current, earlier)) = turn_starts.split_last() else {
        return ("", Vec::new());
    };
    (
        user_request(current).unwrap_or_default(),
        earlier.iter().filter_map(user_request).collect(),
    )
}

async fn ask_approval(
    gate: Gate<'_>,
    turn_id: TurnId,
    judged: &Judged<'_>,
    events: EventSink<'_>,
    cancel: &CancellationToken,
) -> Verdict {
    let Some(approvals) = gate.approvals else {
        return Verdict::Blocked;
    };
    let scope = gate.permissions.approval_scope(judged.action);
    let mut pending = approvals.open();
    events(UiEvent::ApprovalRequested {
        turn_id,
        request: Box::new(approval_request(pending.id(), judged, &scope)),
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
        Some(ApprovalDecision::Once | ApprovalDecision::Always) => Verdict::Run(scope.access),
    }
}

fn approval_request(id: RequestId, judged: &Judged<'_>, scope: &ApprovalScope) -> ApprovalRequest {
    let call = judged.call;
    let (command, file) = match judged.action {
        GatedAction::Call(_) | GatedAction::McpTool(_) => (None, None),
        GatedAction::FileMutation(mutation) => (None, Some(mutation.clone())),
        GatedAction::Command(command) => (Some(command.clone()), None),
    };
    let preview = encode_terminal_safe(call.arguments.as_bytes(), MAX_TOOL_ARGUMENTS_PREVIEW_BYTES);
    ApprovalRequest {
        id,
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        description: judged.description.clone(),
        tool_arguments_preview: preview.text,
        tool_arguments_truncated: preview.truncated,
        scope: scope.clone(),
        command,
        file,
        origin: ApprovalOrigin::ActiveSession,
        change: judged.evidence.file.as_ref().map(FileChange::to_proposed),
    }
}

async fn run_group<'c>(
    turn_id: TurnId,
    group: Vec<(&'c ToolCall, Prepared)>,
    gate: Gate<'_>,
    reviewing: &mut Reviewing<'_>,
    events: EventSink<'_>,
    cancel: &CancellationToken,
) -> SettledGroup<'c> {
    let mut dispatched = Vec::with_capacity(group.len());
    let (statuses, mut reported) = unbounded_channel();
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
                description,
                output,
            }) => {
                events(tool_rejected(turn_id, call, reason, description, &output));
                dispatched.push((call, Dispatched::Rejected(output, reason)));
            }
            Prepared::Ready(prepared, mut description, mutation, command) => {
                let action = gated_action(call, mutation.as_ref(), command.as_ref(), &*prepared);
                let delegates = description.activity == ToolActivity::Subagent;
                let (admission, evidence) = admission(gate, action, &description, &*prepared);
                let shown_while_reviewed =
                    admission == Admission::ReviewRequired && mutation.is_none();
                if shown_while_reviewed {
                    events(tool_started(turn_id, call, description.clone()));
                }
                let judged = Judged {
                    call,
                    action,
                    description: &description,
                    evidence: &evidence,
                };
                let verdict = judge(
                    gate,
                    reviewing,
                    turn_id,
                    admission,
                    judged,
                    &mut *events,
                    cancel,
                )
                .await;
                if mutation.is_some()
                    && !matches!(verdict, Verdict::Run(_))
                    && let Some(Some(label)) = contained(|| prepared.untargeted_label())
                {
                    description.relabel(label);
                }
                if verdict == Verdict::Blocked {
                    blocked = Some(BlockedCall {
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        title: description.title.clone(),
                    });
                }
                let silent = shown_while_reviewed
                    || verdict == Verdict::Interrupted
                    || (mutation.is_some() && verdict == Verdict::Blocked);
                if !silent {
                    events(tool_started(turn_id, call, description));
                }
                let (held, review_hold) = match verdict {
                    Verdict::Run(path_access) => {
                        let delegation = delegates.then_some(&statuses);
                        let context =
                            reviewing.tool_context(turn_id, call, delegation, path_access, cancel);
                        let task = tokio::spawn(async move { prepared.execute(context).await });
                        dispatched.push((call, Dispatched::Running(task)));
                        continue;
                    }
                    Verdict::Held(output) => (output, true),
                    Verdict::Denied => (tool_permission_denied_json(&call.name), false),
                    Verdict::Blocked | Verdict::Interrupted => {
                        if shown_while_reviewed {
                            events(tool_finished(turn_id, call, None));
                        }
                        discard(prepared);
                        break;
                    }
                };
                discard(prepared);
                dispatched.push((
                    call,
                    Dispatched::Held(ToolOutput::failure(held), review_hold),
                ));
            }
        }
    }
    group.for_each(discard);
    SettledGroup {
        outcomes: {
            drop(statuses);
            settle_group(turn_id, dispatched, &mut reported, events, cancel).await
        },
        blocked,
    }
}

async fn settle_group<'c>(
    turn_id: TurnId,
    dispatched: Vec<(&'c ToolCall, Dispatched)>,
    reported: &mut UnboundedReceiver<ChildStatus>,
    events: EventSink<'_>,
    cancel: &CancellationToken,
) -> Vec<Settled<'c>> {
    let mut grace_deadline = None;
    let mut outcomes = Vec::with_capacity(dispatched.len());
    for (call, dispatched) in dispatched {
        let (output, escalates, review_hold) = match dispatched {
            Dispatched::Rejected(output, reason) => {
                report_context_notices(turn_id, &output, events);
                (
                    Some(output),
                    reason != ToolRejection::MalformedArguments,
                    false,
                )
            }
            Dispatched::Held(output, review_hold) => {
                events(tool_finished(turn_id, call, Some(&output)));
                (Some(output), true, review_hold)
            }
            Dispatched::Running(mut task) => {
                let settling = settle(call, &mut task, cancel, &mut grace_deadline);
                let output = forward_child_statuses(turn_id, settling, reported, events).await;
                if let Some(output) = &output {
                    report_context_notices(turn_id, output, events);
                }
                events(tool_finished(turn_id, call, output.as_ref()));
                (output, true, false)
            }
        };
        outcomes.push(Settled {
            call,
            output,
            escalates,
            review_hold,
        });
    }
    outcomes
}

fn tool_rejected(
    turn_id: TurnId,
    call: &ToolCall,
    reason: ToolRejection,
    description: Option<Box<CallDescription>>,
    output: &ToolOutput,
) -> UiEvent {
    UiEvent::ToolRejected {
        turn_id,
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        arguments: call.arguments.clone(),
        reason,
        description: description.map(|description| *description),
        content: output.content.clone(),
    }
}

fn tool_started(turn_id: TurnId, call: &ToolCall, description: CallDescription) -> UiEvent {
    UiEvent::ToolStarted {
        turn_id,
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        description,
    }
}

fn report_context_notices(turn_id: TurnId, output: &ToolOutput, events: EventSink<'_>) {
    for text in &output.context_notices {
        events(UiEvent::ContextNotice {
            turn_id,
            text: text.clone(),
        });
    }
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
        process: output.and_then(|output| output.process),
        status_detail: output.and_then(|output| output.status_detail),
        file_change: output.and_then(|output| output.file_change),
    }
}

type ChildStatus = (ToolCallId, SubagentStatus);

async fn forward_child_statuses<T>(
    turn_id: TurnId,
    settling: impl Future<Output = T>,
    reported: &mut UnboundedReceiver<ChildStatus>,
    events: EventSink<'_>,
) -> T {
    tokio::pin!(settling);
    loop {
        tokio::select! {
            biased;
            Some((call_id, status)) = reported.recv() => events(UiEvent::SubagentStatus {
                turn_id,
                call_id,
                status,
            }),
            settled = &mut settling => return settled,
        }
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
