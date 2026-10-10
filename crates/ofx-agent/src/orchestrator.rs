use std::any::Any;
use std::borrow::Cow;
use std::collections::HashMap;
use std::iter;
use std::mem;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ofx_contract::{
    ActiveMode, Admission, ApprovalDecision, ApprovalOrigin, ApprovalRequest, ApprovalScope,
    AutoCompactPercent, BoxFuture, CallDescription, CapabilityLookup, CapabilityResolver,
    ChatMessage, CommandRequest, Completion, Concurrency, ConversationLog,
    DEFAULT_MAX_TOOL_RESULT_BYTES, DynamicTools, ExecutionFailure, FileChange, FileMutation,
    FinishReason, GatedAction, HookScope, HookView, LogFailure, McpServersCatalog,
    McpServersSection, ModelCapabilities, ModelFailureDiagnostic, ModelProvider,
    ModelRecoveryAction, ModelRecoveryCause, ModelRecoveryRequiredAction, ModelRequest, PathAccess,
    PermissionGate, PreparedCall, ProviderError, ProviderErrorKind, ProviderOptions,
    RecordedOutput, RecoveredTurn, RecoveryStrategy, RequestId, ReviewFailure, ReviewHold,
    ReviewRequest, ReviewVerdict, Reviewed, RootUserRequests, RouteRecoveryKind,
    RouteRecoveryStatus, SkillBinding, StopOutcome, StreamEvent, SubagentStatus,
    SubagentStatusSink, Tool, ToolActivity, ToolArgumentDiagnostic, ToolArgumentIntegrity,
    ToolCall, ToolCallId, ToolContext, ToolEffect, ToolOutput, ToolRejection, ToolResultStatus,
    ToolSpec, TurnId, TurnOutcome, TurnPresentationOutcome, TurnStop, UiEvent, Usage,
    bound_model_output, continuation_message, format_unknown_action, join_visible_segments,
    malformed_tool_arguments_json, non_object_tool_arguments_json, tool_execution_failure_json,
    tool_permission_denied_json, tool_review_held_json,
};
use ofx_text::encode_terminal_safe;
use ofx_trace::{NetworkRing, Ring, TraceContext};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::agent_steps::allows_step;
use crate::approvals::Approvals;
use crate::assistant_stream::normalize_assistant_text_for_display;
use crate::compactor::{CompactionError, CompactionEvent, Payload};
use crate::execution_memory::{EarlierEvidence, partial_view, steering_text};
use crate::gateway_step::Meter;
use crate::lifecycle::{LifecycleContext, ToolPreparation};
use request_usage::RequestUsage;

use crate::model_response_recovery::{
    DEFAULT_MAX_PROVIDER_ATTEMPTS, Decision, Recovery, ToolEvidence, recovery_cause,
};
use crate::project_context::{DeliveryState, ProjectContext, ProjectContextProvider};
use crate::prompt_context::Calibration;
use crate::recovery_pause::RecoveryPause;
use crate::skill_context::{SkillContext, SkillContextFailure, SkillContextProvider};
use crate::tool_admission::{ShellExecutionFailureRetry, ShellValidationRetry};
use crate::tool_call_metrics::{ToolCallOutcome, ToolCallRecord, ToolCallRing};
use crate::trace_rings::TraceRings;
use crate::turn_reviews::TurnReviews;
use crate::worker_runtime::WorkerRuntime;

mod compaction;
mod dynamic_tools;
mod gateway_trace;
mod mode_policy;
mod paused;
mod project_gate;
mod provider_tools;
mod recovery;
mod request_usage;
mod response_language;
mod steering;
mod turn_ledger;
mod turn_log;
mod turn_trace;

pub use compaction::Compaction;
use compaction::{TurnCompaction, compaction_stop};
use dynamic_tools::{DynamicToolSet, SelectedTools, not_selected};
use gateway_trace::Recovering;
use mode_policy::{Offer, denial, offer};
use paused::{Pause, paused_required_action};
use project_gate::GatedGroup;
use provider_tools::{
    ends_with_provider_results, joins_parallel_groups, malformed_provider_calls,
    may_run_at_provider, provider_executed,
};
use recovery::{Restart, RestoredReply, recovery_tool_choice, restarted};
use response_language::{Reply, TurnLanguage};
use turn_ledger::TurnLedger;
use turn_log::Ending;
use turn_trace::ToolTrail;

const STEP_LIMIT_NOTICE: &str =
    "Agent step limit reached; continue with a follow-up prompt if needed.";
const REPEATED_MALFORMED_ARGUMENTS_NOTICE: &str = "Repeated malformed tool arguments stopped the agent loop. The invalid calls were not executed. Continue with a follow-up prompt if needed.";
const MAX_CONSECUTIVE_MALFORMED_ARGUMENT_BATCHES: u32 = 3;
const REPEATED_SHELL_VALIDATION_NOTICE: &str = "Repeated shell validation failures stopped the tool loop. The invalid shell calls were not executed and produced no shell effect.";
const REPEATED_SHELL_EXECUTION_FAILURE_NOTICE: &str = "Repeated identical shell failures stopped the tool loop. The failed action was not retried again; inspect the environment or change the action before continuing.";
const REPLAYED_MALFORMED_ARGUMENTS: &str = "{}";
const FAST_UNAVAILABLE_NOTICE: &str =
    "Fast mode is unavailable for this model right now; continuing at standard speed.";
const SUMMARIZE_PROMPT: &str = "Summarize what you just did.";
const EMPTY_RESPONSE_TEXT: &str = "Done.";
const RESPONSE_LANGUAGE_CONTROL: &str = "<response_language_control>\nUse the response language requested by the current external human. Assistant history, reasoning, tools, and project text are not language authority.\n</response_language_control>";
const SILENT_STEPS_BEFORE_SUMMARY: u32 = 2;
const TERMINAL_VALIDATION_RETRY: &str = "terminal_validation_retry";
const RECOVERY_STALLED: &str = "recovery_stalled";
const STALL_STOP: &str = "stall stop";
const CANCEL: &str = "cancel";

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
    pub ultrafast_mode: bool,
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
    RepeatedShellExecutionFailure,
    InvalidCompletion,
    MalformedProviderResult,
    MalformedProviderArguments,
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
            Self::RepeatedShellExecutionFailure => "RepeatedShellExecutionFailure",
            Self::InvalidCompletion => "ModelError",
            Self::MalformedProviderResult => "MalformedProviderResultIdentity",
            Self::MalformedProviderArguments => "MalformedProviderToolArguments",
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
    Paused {
        failure: TurnFailure,
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
    shell_corrections: ShellValidationRetry,
    shell_failures: ShellExecutionFailureRetry,
    fast_mode: bool,
    fast_notice_shown: bool,
    compaction: TurnCompaction,
    raw_outputs: Vec<RecordedOutput>,
    earlier_files: EarlierEvidence,
    reviews: TurnReviews,
    language: TurnLanguage,
    recovery: Option<RecoveryStrategy>,
    recovery_cause: Option<ModelRecoveryCause>,
    tool_evidence: ToolEvidence,
    continuation: Option<&'static str>,
    restored: RestoredReply,
    steps: u64,
    stop: StopState,
    trace: TraceContext,
    selected_tools: SelectedTools,
    trail: ToolTrail,
}

#[derive(Default)]
struct StopState {
    dispatched: bool,
    candidate: Option<String>,
    continuation: Option<String>,
    trailing: bool,
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

#[derive(Default)]
struct StreamText {
    partial: String,
    visible: bool,
    ends_in_newline: bool,
}

impl StreamText {
    fn push(&mut self, text: &str) {
        self.partial.push_str(text);
    }

    fn displayed(&mut self, text: &str) {
        if !text.is_empty() {
            self.visible |= !text.trim_matches([' ', '\t', '\r', '\n']).is_empty();
            self.ends_in_newline = text.ends_with('\n');
        }
    }
}

struct LastReply {
    turn: usize,
    text: Arc<str>,
}

fn read_tools(tools: &[Arc<dyn Tool>]) -> (Vec<ToolSpec>, Vec<bool>) {
    tools
        .iter()
        .map(|tool| (tool.spec().clone(), tool.provider_executed()))
        .unzip()
}

pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    tools: Vec<Arc<dyn Tool>>,
    tool_specs: Vec<ToolSpec>,
    provider_executed: Vec<bool>,
    offered_specs: Vec<ToolSpec>,
    tool_guidance: String,
    dynamic: Option<DynamicToolSet>,
    mcp_servers: Option<Arc<dyn McpServersCatalog>>,
    mode: Option<ActiveMode>,
    context: Arc<dyn RuntimeContext>,
    permissions: Arc<dyn PermissionGate>,
    approvals: Option<Approvals>,
    reviews_fall_back_to_approval: bool,
    config: AgentConfig,
    capability_resolver: Option<Arc<dyn CapabilityResolver>>,
    capabilities: Option<KnownCapabilities>,
    project: Option<ProjectInstructions>,
    skills: Option<Arc<dyn SkillContextProvider>>,
    history: Vec<ChatMessage>,
    turn_starts: Vec<usize>,
    pending_interruptions: Vec<std::ops::Range<usize>>,
    inherited_requests: Option<Arc<RootUserRequests>>,
    ledger: TurnLedger,
    compacted: Option<Payload>,
    compactions: usize,
    calibration: Option<Calibration>,
    session_id: Option<String>,
    log: Option<Box<dyn ConversationLog>>,
    request_fixed_tokens: Option<usize>,
    turns: u64,
    last_reply: Option<LastReply>,
    steering: Option<Arc<WorkerRuntime>>,
    recovery_pause: RecoveryPause,
    lifecycle: Option<LifecycleContext>,
    compaction_trace: &'static Ring<CompactionEvent>,
    tool_call_trace: &'static ToolCallRing,
    network_calls: &'static NetworkRing,
    next_trace: Option<TraceContext>,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        tools: Vec<Arc<dyn Tool>>,
        context: Arc<dyn RuntimeContext>,
        permissions: Arc<dyn PermissionGate>,
        config: AgentConfig,
    ) -> Self {
        let (tool_specs, provider_executed) = read_tools(&tools);
        let rings = TraceRings::process();
        let Offer {
            specs: offered_specs,
            guidance: tool_guidance,
        } = offer(&tool_specs, tool_specs.len(), &provider_executed, None);
        Self {
            provider,
            tools,
            tool_specs,
            provider_executed,
            offered_specs,
            tool_guidance,
            dynamic: None,
            mcp_servers: None,
            mode: None,
            context,
            permissions,
            approvals: None,
            reviews_fall_back_to_approval: false,
            config,
            capability_resolver: None,
            capabilities: None,
            project: None,
            skills: None,
            history: Vec::new(),
            turn_starts: Vec::new(),
            pending_interruptions: Vec::new(),
            inherited_requests: None,
            ledger: TurnLedger::default(),
            compacted: None,
            compactions: 0,
            calibration: None,
            session_id: None,
            log: None,
            request_fixed_tokens: None,
            turns: 0,
            last_reply: None,
            steering: None,
            recovery_pause: RecoveryPause::default(),
            lifecycle: None,
            compaction_trace: rings.compaction,
            tool_call_trace: rings.tool_calls,
            network_calls: rings.network,
            next_trace: None,
        }
    }

    #[must_use]
    pub fn with_trace_rings(mut self, rings: TraceRings) -> Self {
        self.compaction_trace = rings.compaction;
        self.tool_call_trace = rings.tool_calls;
        self.network_calls = rings.network;
        self
    }

    #[must_use]
    pub fn with_mode(mut self, mode: ActiveMode) -> Self {
        self.mode = Some(mode);
        self.offer_tools();
        self
    }

    #[must_use]
    pub fn with_approvals(mut self, approvals: Approvals) -> Self {
        self.approvals = Some(approvals);
        self.reviews_fall_back_to_approval = true;
        self
    }

    #[must_use]
    pub fn with_permission_prompts(mut self, approvals: Approvals) -> Self {
        self.approvals = Some(approvals);
        self.reviews_fall_back_to_approval = false;
        self
    }

    #[must_use]
    pub fn with_mcp_servers(mut self, catalog: Arc<dyn McpServersCatalog>) -> Self {
        self.mcp_servers = Some(catalog);
        self
    }

    #[must_use]
    pub fn with_dynamic_tools(mut self, source: Arc<dyn DynamicTools>) -> Self {
        self.dynamic = Some(DynamicToolSet::new(source));
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

    #[must_use]
    pub fn with_lifecycle(mut self, hooks: HookView, scope: HookScope) -> Self {
        self.lifecycle = Some(LifecycleContext::new(hooks, scope));
        self
    }

    pub(crate) fn config(&self) -> &AgentConfig {
        &self.config
    }

    pub(crate) fn replace_tools(&mut self, tools: Vec<Arc<dyn Tool>>) {
        (self.tool_specs, self.provider_executed) = read_tools(&tools);
        self.tools = tools;
        if let Some(set) = &mut self.dynamic {
            set.forget_advertised();
        }
        self.offer_tools();
    }

    fn offer_tools(&mut self) {
        let Offer { specs, guidance } = offer(
            &self.tool_specs,
            self.tools.len(),
            &self.provider_executed,
            self.mode.as_ref(),
        );
        self.offered_specs = specs;
        self.tool_guidance = guidance;
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

    pub fn set_provider(
        &mut self,
        provider: Arc<dyn ModelProvider>,
        resolver: Option<Arc<dyn CapabilityResolver>>,
    ) {
        self.provider = provider;
        self.capability_resolver = resolver;
        self.capabilities = None;
        self.calibration = None;
        self.request_fixed_tokens = None;
    }

    pub fn clear_history(&mut self) {
        self.pending_interruptions.clear();
        self.history.clear();
        self.turn_starts.clear();
        self.ledger.reset(0);
        self.compacted = None;
        self.compactions = 0;
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

    pub(crate) fn trace_next_turn_as_subagent(&mut self) -> TraceContext {
        let trace = TraceContext {
            turn_id: ofx_trace::next_turn_id(),
            subagent_id: ofx_trace::next_subagent_id(),
            ..TraceContext::default()
        };
        self.next_trace = Some(trace);
        trace
    }

    fn new_turn(&self, id: TurnId, prompt: &str, trace: TraceContext) -> Turn {
        Turn {
            id,
            start: self.history.len(),
            usage: Usage::default(),
            silent_tool_steps: 0,
            summary_requested: false,
            failures: HashMap::new(),
            malformed_batches: 0,
            shell_corrections: ShellValidationRetry::default(),
            shell_failures: ShellExecutionFailureRetry::default(),
            fast_mode: self.config.fast_mode,
            fast_notice_shown: false,
            compaction: TurnCompaction::default(),
            raw_outputs: Vec::new(),
            earlier_files: EarlierEvidence::default(),
            reviews: TurnReviews::default(),
            language: self.turn_language(prompt),
            recovery: None,
            recovery_cause: None,
            tool_evidence: ToolEvidence::None,
            continuation: None,
            restored: RestoredReply::default(),
            steps: 0,
            stop: StopState::default(),
            trace,
            selected_tools: SelectedTools::default(),
            trail: ToolTrail::default(),
        }
    }

    fn restore_recovered(&mut self, turn: &mut Turn, recovered: RecoveredTurn) {
        turn.earlier_files = EarlierEvidence::recovered(
            recovered.files,
            recovered
                .messages
                .iter()
                .filter(|message| matches!(message, ChatMessage::Tool { .. }))
                .count(),
        );
        self.history.extend(recovered.messages);
        turn.raw_outputs = recovered.outputs;
        turn.fast_mode = recovered.fast_mode;
        turn.recovery = Some(recovered.strategy);
        turn.recovery_cause = recovered.cause;
        turn.tool_evidence = ToolEvidence::restored(recovered.tool_state);
        turn.restored = RestoredReply {
            source: recovered.source,
            presented: recovered.source_presented,
        };
    }

    async fn run_prompt(
        &mut self,
        prompt: &str,
        skills: &[SkillBinding],
        recovered: Option<RecoveredTurn>,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> TurnReport {
        self.settle_lifecycle().await;
        self.turns += 1;
        self.recovery_pause.reset();
        let id = TurnId::new(self.turns);
        events(UiEvent::TurnStarted { turn_id: id });
        let trace = self.next_trace.take().unwrap_or_else(|| TraceContext {
            turn_id: ofx_trace::next_turn_id(),
            ..TraceContext::default()
        });
        if let Err(failure) = self.require_writable() {
            turn_trace::prompt_failed_before_start(trace, prompt, &self.config.model);
            events(UiEvent::TurnFinished {
                turn_id: id,
                outcome: TurnOutcome::Failed,
            });
            self.post_turn_end(id, TurnPresentationOutcome::Failed);
            return TurnReport {
                outcome: TurnOutcome::Failed,
                final_text: String::new(),
                usage: Usage::default(),
                failure: Some(TurnFailure::Persistence(failure)),
            };
        }
        self.close_interrupted_turns(self.continues_steering());
        let mut turn = self.new_turn(id, prompt, trace);
        self.turn_starts.push(turn.start);
        self.history.push(self.turn_message(prompt));
        if let Some(recovered) = recovered {
            self.restore_recovered(&mut turn, recovered);
        }
        turn_trace::prompt_start(turn.trace, prompt, &self.config.model);
        let result = self.drive(&mut turn, prompt, skills, events, cancel).await;
        let outcome_kind = turn_trace::outcome_kind(&result, &turn.trail, cancel.is_cancelled());
        let (outcome, final_text, mut failure, ending) = self.settle_result(&turn, prompt, result);
        if let Err(error) = self.save_turn(prompt, &turn, ending)
            && failure.is_none()
        {
            failure = Some(TurnFailure::Persistence(error));
        }
        self.forget_summary_prompt(&turn);
        self.forget_stop_continuation(&turn);
        self.hold_interruption(ending, turn.start);
        events(UiEvent::TurnFinished {
            turn_id: id,
            outcome,
        });
        if outcome_kind == turn_trace::CANCELLED {
            turn_trace::cancelled_turn(turn.trace);
        }
        turn_trace::prompt_finish(turn.trace, outcome_kind);
        if outcome == TurnOutcome::Completed {
            self.last_reply = Some(LastReply {
                turn: self.turn_starts.len().saturating_sub(1),
                text: Arc::from(final_text.as_str()),
            });
        }
        self.post_turn_end(id, presentation_outcome(outcome, ending));
        TurnReport {
            outcome,
            final_text,
            usage: turn.usage,
            failure,
        }
    }

    fn settle_result(
        &mut self,
        turn: &Turn,
        prompt: &str,
        result: Result<String, Stop>,
    ) -> (TurnOutcome, String, Option<TurnFailure>, Ending) {
        match result {
            Ok(text) => (TurnOutcome::Completed, text, None, Ending::Replied),
            Err(Stop::Interrupted { partial }) => {
                self.keep_partial_turn(turn.start, &partial);
                turn_trace::interrupted(turn.trace, prompt, &partial, &turn.trail);
                (
                    TurnOutcome::Interrupted,
                    String::new(),
                    None,
                    Ending::Stopped(TurnStop::Cancelled),
                )
            }
            Err(Stop::Paused { failure }) => {
                self.history.truncate(turn.start);
                self.turn_starts.pop();
                (
                    TurnOutcome::Failed,
                    String::new(),
                    Some(failure),
                    Ending::Paused,
                )
            }
            Err(Stop::Failed { failure, partial }) => {
                let spoke = !partial.trim_matches(TRIMMED).is_empty();
                let ending = if turn.stop.candidate.is_some() {
                    self.keep_partial_turn(turn.start, &partial);
                    Ending::Replied
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
                    turn_trace::stream_failure_persisted(turn.trace, prompt, &partial);
                    Ending::Stopped(TurnStop::Failed)
                } else {
                    self.keep_partial_turn(turn.start, "");
                    Ending::Replied
                };
                (TurnOutcome::Failed, String::new(), Some(failure), ending)
            }
        }
    }

    pub async fn settle_lifecycle(&mut self) {
        if let Some(lifecycle) = &mut self.lifecycle {
            lifecycle.settle().await;
        }
    }

    fn post_turn_end(&mut self, turn_id: TurnId, outcome: TurnPresentationOutcome) {
        if let Some(lifecycle) = &mut self.lifecycle {
            lifecycle.post_turn_end(turn_id, outcome);
        }
    }

    async fn stop_checkpoint(
        &mut self,
        turn: &mut Turn,
        reply: Option<String>,
        can_continue: bool,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, Stop> {
        let Some(text) = reply else {
            return Ok(None);
        };
        let Some(lifecycle) = self
            .lifecycle
            .as_ref()
            .filter(|lifecycle| lifecycle.has_stop() && !turn.stop.dispatched)
        else {
            return Ok(Some(text));
        };
        turn.stop.dispatched = true;
        turn.stop.trailing = true;
        let normalized = normalize_assistant_text_for_display(&text);
        let rendered = if normalized.is_empty() {
            EMPTY_RESPONSE_TEXT
        } else {
            &normalized
        };
        let step_index = usize::try_from(turn.steps).unwrap_or(usize::MAX);
        match lifecycle
            .stop(turn.id, step_index, rendered, can_continue, cancel)
            .await
        {
            None => Err(Stop::interrupted()),
            Some(StopOutcome::Allow) => Ok(Some(text)),
            Some(StopOutcome::ContinueOnce(context)) => {
                let continuation = continuation_message(&context);
                self.history.push(ChatMessage::user(continuation.clone()));
                turn.stop = StopState {
                    dispatched: true,
                    candidate: Some(text),
                    continuation: Some(continuation),
                    trailing: false,
                };
                events(UiEvent::AssistantBoundary { turn_id: turn.id });
                Ok(None)
            }
        }
    }

    fn forget_stop_continuation(&mut self, turn: &Turn) {
        let Some(continuation) = &turn.stop.continuation else {
            return;
        };
        let start = (turn.start + 1).min(self.history.len());
        if let Some(index) = self.history[start..].iter().position(|message| {
            matches!(message, ChatMessage::User { content, .. } if content == continuation)
        }) {
            self.history.remove(start + index);
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
        let servers = self.mcp_servers_section(turn.id, events);
        let mut step = 0;
        loop {
            self.stop_at_step_limit(turn, step, events)?;
            let entered = enter_step(turn, step);
            let step_cancel = self.begin_model_step(turn, events, cancel)?;
            if self.has_compactable_context(turn) {
                self.resolve_capabilities(cancel).await?;
            }
            self.refresh_dynamic_tools(&turn.selected_tools);
            let context = self.context.runtime_context().await;
            let instructions = self.instructions(&skills, &context, &servers);
            let messages = self.request_messages(turn);
            let gateway_messages = instructions.len() + messages.len();
            self.trace_step(turn, (step + 1, entered), gateway_messages);
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
            self.history
                .extend(turn.continuation.take().map(ChatMessage::user));
            let completion = match outcome {
                Ok(completion) => {
                    self.trace_request_after_compaction(
                        turn,
                        measured.as_ref(),
                        completion.usage.input_tokens,
                    );
                    self.settle_measurement(measured, completion.usage.input_tokens);
                    turn.recovery = None;
                    turn.recovery_cause = None;
                    turn.tool_evidence = ToolEvidence::None;
                    completion
                }
                Err(Stop::Failed {
                    failure: TurnFailure::Provider(error),
                    partial,
                }) if self.recovers_overflow(turn, &error, &partial, measured.as_ref(), cancel) => {
                    self.settle_measurement(measured, None);
                    continue;
                }
                Err(Stop::Interrupted { partial }) if self.steers_after_interrupt(cancel) => {
                    self.settle_measurement(measured, None);
                    turn.recovery = None;
                    turn.recovery_cause = None;
                    turn.tool_evidence = ToolEvidence::None;
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
                    Reply::Accepted(completion) => *completion,
                    Reply::Steered => {
                        step += 1;
                        continue;
                    }
                    Reply::Rejected => continue,
                };
            let finish = settled_finish(turn, &completion)?;
            turn_trace::step_completion(turn.trace, step + 1, &completion);
            step += 1;
            turn.steps = step;
            let more_steps = allows_step(self.config.step_limit, step);
            if let Some(text) = self
                .settle_completion(turn, (completion, finish), more_steps, events, cancel)
                .await?
            {
                return Ok(text);
            }
        }
    }

    async fn settle_completion(
        &mut self,
        turn: &mut Turn,
        (completion, finish): (Completion, Finish),
        more_steps: bool,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, Stop> {
        match finish {
            Finish::Reply => {
                let reply = self.finish(turn, completion, more_steps, events)?;
                self.stop_checkpoint(turn, reply, more_steps, events, cancel)
                    .await
            }
            Finish::ProviderResults => {
                let reply =
                    self.finish_with_provider_results(turn, completion, more_steps, events)?;
                self.stop_checkpoint(turn, reply, more_steps, events, cancel)
                    .await
            }
            Finish::Batch => {
                self.run_batch(turn, completion, more_steps, events, cancel)
                    .await
            }
        }
    }

    fn mcp_servers_section(&self, turn_id: TurnId, events: EventSink<'_>) -> McpServersSection {
        let Some(catalog) = &self.mcp_servers else {
            return McpServersSection::default();
        };
        let mut section = catalog.section();
        if let Some(text) = section.notice.take() {
            events(UiEvent::ContextNotice { turn_id, text });
        }
        section
    }

    fn instructions<'a>(
        &'a self,
        skills: &'a SkillContext,
        context: &'a [String],
        servers: &'a McpServersSection,
    ) -> Vec<&'a str> {
        let deltas = self
            .project
            .as_ref()
            .map_or(0, |project| project.deltas.len());
        let mut instructions: Vec<&str> = Vec::with_capacity(context.len() + deltas + 8);
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
        }
        if !servers.text.is_empty() {
            instructions.push(&servers.text);
        }
        instructions.extend(servers.change_notice.as_deref());
        if let Some(project) = &self.project {
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
            tools: &self.offered_specs,
            tool_choice: recovery_tool_choice(turn.recovery),
            max_output_tokens: self.config.max_output_tokens,
            provider_options: self.provider_options(turn, events),
            session_id: self.session_id.as_deref(),
        }
    }

    fn provider_options(&self, turn: &mut Turn, events: EventSink<'_>) -> ProviderOptions<'_> {
        let options = self.selected_provider_options(turn, events);
        turn_trace::provider_options(
            turn.trace,
            &turn_trace::ProviderOptionsTrace {
                model: &self.config.model,
                fast_mode: turn.fast_mode,
                effort: self.config.reasoning_effort.as_deref(),
                reasoning_selected: options.reasoning_effort.is_some(),
                fast_selected: options.fast,
            },
        );
        options
    }

    fn selected_provider_options(
        &self,
        turn: &mut Turn,
        events: EventSink<'_>,
    ) -> ProviderOptions<'_> {
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
        let mut recovery = Recovery::default();
        let mut recovering_from = None;
        let mut pending = None;
        let mut restart = Restart::begin(turn, request.messages, events);
        loop {
            let sent = ModelRequest {
                messages: restart.messages(),
                ..request
            };
            let Attempt {
                streamed,
                partial,
                streamed_bytes,
                admitted,
                tool,
                settled,
            } = self
                .attempt(turn, &sent, body.take(), &mut pending, events, cancel)
                .await?;
            let consumed = attempt - usize::from(!admitted);
            let counted = (attempt, consumed, tool);
            let observed = restart.observe(partial, counted, &mut turn.tool_evidence);
            if let Err(failure) = settled {
                return Err(restart.failed(TurnFailure::Persistence(failure)));
            }
            let error = match streamed {
                Ok(completion) => {
                    let outcome = (recovering_from.is_some(), attempt, tool);
                    return self.completed(turn, completion, outcome, &restart, events);
                }
                Err(error) => error,
            };
            let failed = (&error, observed, cancel);
            if error.kind == ProviderErrorKind::Cancelled || cancel.is_cancelled() {
                self.trace_failure(turn, &restart, failed, Recovering::Cancelled);
                let recovery = recovering_from.map(|cause| (cause, consumed));
                return Err(self.interruption(turn, recovery, &error, restart, events));
            }
            self.reconcile_broken(turn, failed, &restart, events)?;
            let Some(cause) = recovery_cause(error.kind) else {
                if self.asks_again(turn, (&error, tool, attempt), &mut restart) {
                    recovery.reset_pacing();
                    attempt += 1;
                    continue;
                }
                self.trace_failure(turn, &restart, failed, Recovering::Stop);
                if let Some(status) =
                    stopped_status(recovering_from, attempt, consumed, &error, observed.spoke)
                {
                    events(UiEvent::Recovery { turn_id, status });
                }
                return Err(restart.failed(TurnFailure::Provider(error)));
            };
            let evidence = (tool, cause, &error);
            let evidence =
                restart.evidence(&mut turn.tool_evidence, evidence, &turn.language.stage);
            let decision = recovery.decide(cause, &error, streamed_bytes, evidence);
            let Some(action) = decision.strategy.action() else {
                self.trace_failure(turn, &restart, failed, Recovering::Stall);
                turn.trail.finish = Some(RECOVERY_STALLED);
                events(UiEvent::Recovery {
                    turn_id,
                    status: stalled_status(cause, consumed, &error, &decision),
                });
                self.discard_recovery(STALL_STOP);
                return Err(restart.failed(TurnFailure::Provider(error)));
            };
            let decided = (cause, decision.strategy);
            self.prepare_retry(turn, &mut request, &mut restart, failed, decided);
            let mut status = retry_status(attempt, cause, action, &decision, &error);
            self.record_wait(turn, cause, action, consumed, &restart)?;
            events(UiEvent::Recovery {
                turn_id,
                status: status.clone(),
            });
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    let recovery = Some((cause, consumed));
                    return Err(self.interruption(turn, recovery, &error, restart, events));
                }
                () = tokio::time::sleep(decision.delay) => {}
            }
            if restart.restarted(&turn.language.stage) {
                events(restarted(turn_id));
            }
            turn.language.stage.restart();
            attempt += 1;
            status.failed_attempt = attempt;
            status.retry_wait = None;
            pending = Some(status);
            recovering_from = Some(cause);
        }
    }

    async fn attempt(
        &self,
        turn: &mut Turn,
        request: &ModelRequest<'_>,
        body: Option<String>,
        pending: &mut Option<RouteRecoveryStatus>,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<Attempt, Stop> {
        let turn_id = turn.id;
        let trace = turn.trace;
        let mut streamed_text = StreamText::default();
        let mut streamed_bytes = 0;
        let mut admitted = false;
        let mut tool = ToolEvidence::None;
        let mut usage = RequestUsage::new(self.log.as_deref());
        let attempt_cancel = cancel.child_token();
        let mut sink = |event: StreamEvent| match event {
            StreamEvent::Admitted => {
                admitted = true;
                turn_trace::provider_admitted(trace, request.model);
                if !usage.admit() {
                    attempt_cancel.cancel();
                }
                if let Some(status) = pending.take() {
                    events(UiEvent::Recovery { turn_id, status });
                }
            }
            StreamEvent::ToolCallStarted { call_id, tool_name } => {
                if may_run_at_provider(&tool_name, &self.tool_specs, &self.provider_executed) {
                    tool = ToolEvidence::Uncertain;
                } else if tool == ToolEvidence::None {
                    tool = ToolEvidence::ProvenUnexecuted;
                }
                streamed_text.ends_in_newline |= self.streamed_tool_start(
                    turn_id,
                    call_id,
                    tool_name,
                    streamed_text.visible && !streamed_text.ends_in_newline,
                    events,
                );
            }
            StreamEvent::TextDelta { text } => {
                streamed_bytes += text.len();
                streamed_text.push(&text);
                if let Some(text) = turn.language.stage.admit(text) {
                    streamed_text.displayed(&text);
                    events(UiEvent::AssistantText { turn_id, text });
                }
            }
            StreamEvent::ToolInputDelta { text } => {
                self.enter_tool_phase();
                streamed_bytes += text.len();
            }
            StreamEvent::ReasoningDelta { text } => {
                streamed_bytes += text.len();
                events(UiEvent::ReasoningText { turn_id, text });
            }
        };
        let started_at_ms = ofx_trace::timestamp_ms();
        let streamed = match body {
            Some(body) => {
                self.provider
                    .stream_body(request, body, &mut sink, &attempt_cancel)
                    .await
            }
            None => {
                self.provider
                    .stream(request, &mut sink, &attempt_cancel)
                    .await
            }
        };
        Meter::new(self.network_calls, trace).record(request.model, started_at_ms, &streamed);
        let settled = usage.settle(&streamed);
        Ok(Attempt {
            streamed,
            partial: streamed_text.partial,
            streamed_bytes,
            admitted,
            tool,
            settled,
        })
    }

    pub fn recovery_pause(&self) -> RecoveryPause {
        self.recovery_pause.clone()
    }

    fn interruption(
        &self,
        turn: &Turn,
        recovery: Option<(ModelRecoveryCause, usize)>,
        error: &ProviderError,
        restart: Restart<'_>,
        events: EventSink<'_>,
    ) -> Stop {
        let Some((cause, attempt)) = recovery else {
            return Stop::Interrupted {
                partial: restart.into_partial(),
            };
        };
        if !self.recovery_pause.requested() {
            self.discard_recovery(CANCEL);
            return Stop::Interrupted {
                partial: restart.into_partial(),
            };
        }
        let pause = Pause {
            cause,
            attempt,
            required_action: paused_required_action(turn.tool_evidence),
            diagnostic: failure_diagnostic(error),
        };
        self.pause(turn, pause, &restart, events)
    }

    fn streamed_tool_start(
        &self,
        turn_id: TurnId,
        call_id: ToolCallId,
        tool_name: String,
        needs_newline: bool,
        events: EventSink<'_>,
    ) -> bool {
        self.enter_tool_phase();
        let Some(tool) = self.tool(&tool_name) else {
            return false;
        };
        if needs_newline {
            events(UiEvent::AssistantBoundary { turn_id });
        }
        let presentation = contained(|| tool.provisional_presentation()).flatten();
        if let Some(presentation) = presentation.filter(|_| !call_id.as_str().is_empty()) {
            events(UiEvent::ToolProvisional {
                turn_id,
                call_id,
                tool_name,
                action_label: presentation.action_label.to_owned(),
            });
        }
        needs_newline
    }

    async fn run_batch(
        &mut self,
        turn: &mut Turn,
        completion: Completion,
        more_steps: bool,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, Stop> {
        self.enter_tool_phase();
        turn.trail.ran_tools = true;
        turn.trail.step_text_bytes = completion.content.as_deref().map_or(0, str::len);
        turn.shell_corrections.begin_batch();
        turn.shell_failures.begin_batch();
        turn.silent_tool_steps = if completion
            .content
            .as_deref()
            .is_some_and(|text| !text.is_empty())
        {
            0
        } else {
            turn.silent_tool_steps + 1
        };
        let malformed: Vec<Option<ToolOutput>> = completion
            .tool_calls
            .iter()
            .map(|call| {
                if provider_executed(call) {
                    None
                } else {
                    argument_rejection(call)
                }
            })
            .collect();
        let all_malformed = !malformed.is_empty() && malformed.iter().all(Option::is_some);
        let Some(hooked) = self
            .pre_tool_use(turn, &completion.tool_calls, &malformed, cancel)
            .await
        else {
            turn.trail.cancelled_in_tools = true;
            return Err(Stop::Interrupted {
                partial: completion.content.unwrap_or_default(),
            });
        };
        let (calls, mut rejected) = self.record_tool_step(completion, malformed, hooked);
        let mut feedback = Vec::new();
        let ran = self
            .run_groups(turn, &calls, &mut rejected, &mut feedback, events, cancel)
            .await;
        self.history.extend(
            feedback
                .into_iter()
                .map(|(call_id, text)| ChatMessage::permission_feedback(call_id, text)),
        );
        turn.trail.cancelled_in_tools = matches!(ran, Err(Stop::Interrupted { .. }));
        ran?;
        self.settle_batch_retries(turn, all_malformed, calls.len(), more_steps, events)
    }

    fn settle_batch_retries(
        &mut self,
        turn: &mut Turn,
        all_malformed: bool,
        calls: usize,
        more_steps: bool,
        events: EventSink<'_>,
    ) -> Result<Option<String>, Stop> {
        turn.malformed_batches = if all_malformed {
            (turn.malformed_batches + 1).min(MAX_CONSECUTIVE_MALFORMED_ARGUMENT_BATCHES)
        } else {
            0
        };
        let corrections_repeated = turn.shell_corrections.finish_batch();
        let failures_repeated = turn.shell_failures.finish_batch();
        if turn.malformed_batches == MAX_CONSECUTIVE_MALFORMED_ARGUMENT_BATCHES {
            if self.steered_at_finalizing(turn.id, more_steps, events) {
                return Ok(None);
            }
            turn_trace::repeated_tool_failure(
                turn.trace,
                &TurnFailure::RepeatedMalformedArguments,
                calls,
            );
            return Err(self.stop_with_notice(
                turn.id,
                events,
                REPEATED_MALFORMED_ARGUMENTS_NOTICE,
                TurnFailure::RepeatedMalformedArguments,
            ));
        }
        if corrections_repeated {
            if self.steered_at_finalizing(turn.id, more_steps, events) {
                return Ok(None);
            }
            events(UiEvent::SystemNotice {
                text: REPEATED_SHELL_VALIDATION_NOTICE.to_owned(),
            });
            turn.trail.finish = Some(TERMINAL_VALIDATION_RETRY);
            return Ok(Some(String::new()));
        }
        if failures_repeated {
            if self.steered_at_finalizing(turn.id, more_steps, events) {
                return Ok(None);
            }
            turn_trace::repeated_tool_failure(
                turn.trace,
                &TurnFailure::RepeatedShellExecutionFailure,
                calls,
            );
            return Err(self.stop_with_notice(
                turn.id,
                events,
                REPEATED_SHELL_EXECUTION_FAILURE_NOTICE,
                TurnFailure::RepeatedShellExecutionFailure,
            ));
        }
        Ok(None)
    }

    fn steered_at_finalizing(
        &mut self,
        turn_id: TurnId,
        more_steps: bool,
        events: EventSink<'_>,
    ) -> bool {
        let Some(steering) = more_steps.then(|| self.finalizing_steering()).flatten() else {
            return false;
        };
        self.append_steering(turn_id, steering, events);
        true
    }

    async fn run_groups(
        &mut self,
        turn: &mut Turn,
        calls: &[ToolCall],
        rejected: &mut [Option<Rejection>],
        feedback: &mut Vec<(ToolCallId, String)>,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), Stop> {
        let mut gate = match self.project {
            Some(_) => Some(self.open_gate(turn.id, calls, rejected, events, cancel)?),
            None => None,
        };
        let mut next = 0;
        let mut carried = Deferred(None);
        while next < calls.len() {
            let call = &calls[next];
            if cancel.is_cancelled() {
                turn_trace::tool_call(turn.trace, call);
                turn.trail.called(call);
                turn.trail.interrupted_at(call);
                return Err(Stop::interrupted());
            }
            if provider_executed(call) {
                self.publish_provider_result(turn, call, events);
                next += 1;
                continue;
            }
            let group = match &mut gate {
                Some(gate) => match self.gated_group(gate, calls, next) {
                    GatedGroup::Run(group) => group,
                    GatedGroup::Unexecuted(description, output) => {
                        turn_trace::tool_call(turn.trace, call);
                        turn.trail.called(call);
                        turn.raw_outputs
                            .push(partial_view(call.id.clone(), output.len()));
                        self.settle_unexecuted(turn.id, call, description, output, events);
                        next += 1;
                        continue;
                    }
                },
                None => self.lazy_group(calls, next, rejected, &mut carried),
            };
            next += group.len();
            self.settle_one_group(turn, calls, group, feedback, events, cancel)
                .await?;
        }
        if cancel.is_cancelled() {
            return Err(Stop::interrupted());
        }
        Ok(())
    }

    async fn settle_one_group(
        &mut self,
        turn: &mut Turn,
        calls: &[ToolCall],
        group: Vec<(&ToolCall, Prepared)>,
        feedback: &mut Vec<(ToolCallId, String)>,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), Stop> {
        let parallel = (group.len() > 1)
            .then(|| group[0].1.parallel_group())
            .flatten();
        let gate = Gate {
            permissions: &*self.permissions,
            approvals: self.approvals.as_ref(),
            reviews_fall_back_to_approval: self.reviews_fall_back_to_approval,
        };
        let mut reviewing = Reviewing {
            model: &self.config.model,
            history: &self.history,
            turn_starts: &self.turn_starts,
            compacted_turns: self.compacted.as_ref().map(|payload| payload.turn_count),
            inherited_requests: self.inherited_requests.as_ref(),
            turn_start: turn.start,
            batch: calls,
            reviews: &mut turn.reviews,
            usage: &mut turn.usage,
        };
        let traced = (turn.id, turn.trace, parallel);
        let settled = run_group(traced, group, gate, &mut reviewing, events, cancel).await;
        let executed = settled
            .outcomes
            .iter()
            .filter(|outcome| outcome.ran.is_some())
            .count();
        self.record_settled(turn, settled.outcomes, parallel.is_some(), feedback);
        if let Some(kind) = parallel {
            turn_trace::parallel_group(
                turn.trace,
                "parallel_tool_group_finish",
                kind.name(),
                executed,
            );
        }
        if let Some(call) = settled.stopped_at {
            turn.trail.called(call);
            turn.trail.interrupted_at(call);
        }
        match settled.blocked {
            Some(blocked) => Err(Stop::failed(TurnFailure::PermissionRequired(blocked))),
            None => Ok(()),
        }
    }

    fn record_settled(
        &mut self,
        turn: &mut Turn,
        outcomes: Vec<Settled<'_>>,
        parallel: bool,
        feedback: &mut Vec<(ToolCallId, String)>,
    ) {
        for Settled {
            call,
            output,
            escalates,
            executed,
            review_hold,
            feedback: given,
            ran,
        } in outcomes
        {
            feedback.extend(given.map(|text| (call.id.clone(), text)));
            turn.trail.called(call);
            self.record_tool_call(turn.trace, call, output.as_ref(), ran);
            let interrupted = ran.is_some_and(|ran| ran.cancelled);
            if interrupted {
                turn.trail.interrupted_at(call);
            }
            let Some(output) = output else {
                continue;
            };
            turn.selected_tools.record(&output);
            if executed && let (Some(change), Some(log)) = (output.file_change, &self.log) {
                log.record_committed_lines(change);
            }
            let status = output.status;
            if executed && !interrupted && (status == ToolResultStatus::Success || !parallel) {
                turn.trail.completed(call);
            }
            if executed {
                let saved = self.saved_arguments(call);
                turn.shell_failures.observe(
                    &call.name,
                    saved.as_deref().unwrap_or(&call.arguments),
                    status,
                );
            } else {
                turn.shell_corrections.observe(call, &output.content);
            }
            let shown_whole = output.model_view_covers_full_file == Some(true);
            let bytes = output.content.len();
            let result_kind = turn_trace::result_kind(&output);
            let (model_output, truncated) =
                bound_model_output(&call.name, output.content, DEFAULT_MAX_TOOL_RESULT_BYTES);
            if let Some(ran) = ran.filter(|ran| !ran.cancelled) {
                turn_trace::tool_execution_result(
                    turn.trace,
                    call,
                    result_kind,
                    ran.panicked,
                    model_output.len(),
                );
            }
            turn.raw_outputs.push(RecordedOutput {
                call_id: call.id.clone(),
                bytes,
                whole_file: shown_whole && !truncated,
                process: output.process,
            });
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

    fn record_tool_call(
        &self,
        trace: TraceContext,
        call: &ToolCall,
        output: Option<&ToolOutput>,
        ran: Option<Ran>,
    ) {
        let outcome = match (output, ran) {
            (Some(output), Some(ran)) => ran_outcome(output, ran.panicked),
            (None, Some(_)) => ToolCallOutcome::ToolFailed,
            (Some(_), None) if trace.subagent_id == 0 => ToolCallOutcome::Rejected,
            _ => return,
        };
        let shown = output
            .filter(|_| !ran.is_some_and(|ran| ran.panicked))
            .map_or("", |output| output.content.as_str());
        let now = ofx_trace::timestamp_ms();
        self.tool_call_trace.record(&ToolCallRecord {
            name: &call.name,
            arguments: &call.arguments,
            output: shown,
            outcome,
            started_at_ms: ran.map_or(now, |ran| ran.started_at_ms),
            finished_at_ms: ran.map_or(now, |ran| ran.finished_at_ms),
            subagent_id: trace.subagent_id,
        });
    }

    fn record_tool_step(
        &mut self,
        completion: Completion,
        malformed: Vec<Option<ToolOutput>>,
        hooked: Vec<ToolPreparation>,
    ) -> (Vec<ToolCall>, Vec<Option<Rejection>>) {
        let (calls, rejected): (Vec<ToolCall>, Vec<Option<Rejection>>) = completion
            .tool_calls
            .into_iter()
            .zip(malformed)
            .zip(
                hooked
                    .into_iter()
                    .chain(iter::repeat(ToolPreparation::Unchanged)),
            )
            .map(|((call, malformed), hooked)| prepared_for_history(call, malformed, hooked))
            .unzip();
        let history_calls = calls
            .iter()
            .zip(&rejected)
            .map(|(call, rejection)| match rejection {
                Some(Rejection {
                    reason: ToolRejection::MalformedArguments,
                    ..
                }) => call.clone(),
                _ if provider_executed(call) => call.clone(),
                _ => self.history_call(call.clone()),
            })
            .collect();
        self.history.push(ChatMessage::Assistant {
            content: completion.content,
            tool_calls: history_calls,
            provider_replay: completion.provider_replay,
        });
        (calls, rejected)
    }

    async fn pre_tool_use(
        &self,
        turn: &Turn,
        calls: &[ToolCall],
        malformed: &[Option<ToolOutput>],
        cancel: &CancellationToken,
    ) -> Option<Vec<ToolPreparation>> {
        let Some(lifecycle) = self
            .lifecycle
            .as_ref()
            .filter(|lifecycle| lifecycle.has_pre_tool_use())
        else {
            return Some(Vec::new());
        };
        let step_index = usize::try_from(turn.steps).unwrap_or(usize::MAX);
        let mut prepared = Vec::with_capacity(calls.len());
        for (call, malformed) in calls.iter().zip(malformed) {
            if provider_executed(call) || malformed.is_some() {
                prepared.push(ToolPreparation::Unchanged);
                continue;
            }
            prepared.push(
                lifecycle
                    .pre_tool_use(turn.id, step_index, call, cancel)
                    .await?,
            );
        }
        Some(prepared)
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
        let dynamic = self.dynamic.iter().flat_map(DynamicToolSet::advertised);
        self.tools
            .iter()
            .chain(dynamic)
            .zip(&self.tool_specs)
            .find_map(|(tool, spec)| (spec.name == name).then_some(tool))
    }

    fn trace_step(&self, turn: &mut Turn, (step_index, entered): (u64, bool), messages: usize) {
        turn.trail.gateway_messages = messages;
        if entered {
            turn_trace::step_begin(turn.trace, step_index, self.config.step_limit, messages);
        }
        turn_trace::before_provider_preflight(turn.trace, &self.config.model, messages);
    }

    fn stop_at_step_limit(
        &mut self,
        turn: &Turn,
        step: u64,
        events: EventSink<'_>,
    ) -> Result<(), Stop> {
        if !allows_step(self.config.step_limit, step) {
            turn_trace::step_limit_reached(turn.trace, step, self.config.step_limit, &turn.trail);
            return Err(self.stop_with_notice(
                turn.id,
                events,
                STEP_LIMIT_NOTICE,
                TurnFailure::StepLimitReached,
            ));
        }
        Ok(())
    }

    fn refresh_dynamic_tools(&mut self, selected: &SelectedTools) {
        let Some(set) = &mut self.dynamic else {
            return;
        };
        if !set.advertise(selected) {
            return;
        }
        self.tool_specs.truncate(self.tools.len());
        self.provider_executed.truncate(self.tools.len());
        for tool in set.advertised() {
            self.tool_specs.push(tool.spec().clone());
            self.provider_executed.push(tool.provider_executed());
        }
        self.offer_tools();
    }

    fn saved_arguments(&self, call: &ToolCall) -> Option<String> {
        self.tool(&call.name)
            .and_then(|tool| contained(|| tool.saved_arguments(&call.arguments)).flatten())
    }

    fn history_call(&self, call: ToolCall) -> ToolCall {
        match self.saved_arguments(&call) {
            Some(arguments) => ToolCall { arguments, ..call },
            None => call,
        }
    }

    fn request_history(&self) -> Cow<'_, [ChatMessage]> {
        let mut history = Cow::Borrowed(self.history.as_slice());
        for (index, message) in self.history.iter().enumerate() {
            let ChatMessage::Assistant { tool_calls, .. } = message else {
                continue;
            };
            for (position, call) in tool_calls.iter().enumerate() {
                let sent = self.tool(&call.name).and_then(|tool| {
                    contained(|| tool.request_arguments(&call.arguments)).flatten()
                });
                if let Some(arguments) = sent
                    && let ChatMessage::Assistant { tool_calls, .. } = &mut history.to_mut()[index]
                {
                    tool_calls[position].arguments = arguments;
                }
            }
        }
        history
    }

    fn lazy_group<'c>(
        &self,
        calls: &'c [ToolCall],
        start: usize,
        rejected: &mut [Option<Rejection>],
        carried: &mut Deferred,
    ) -> Vec<(&'c ToolCall, Prepared)> {
        let head = match carried.0.take() {
            Some(uncompleted) => uncompleted.complete(&calls[start].name),
            None => self.prepare(&calls[start], rejected[start].take()),
        };
        let parallel = head
            .parallel_group()
            .filter(|_| joins_parallel_groups(&calls[start]));
        let mut group = vec![(&calls[start], head)];
        for (call, rejected) in calls[start + 1..].iter().zip(&mut rejected[start + 1..]) {
            if parallel.is_none() || !joins_parallel_groups(call) {
                break;
            }
            let uncompleted = self.prepare_uncompleted(call, rejected.take());
            if uncompleted.parallel_group() != parallel {
                carried.0 = Some(uncompleted);
                break;
            }
            group.push((call, uncompleted.complete(&call.name)));
        }
        group
    }

    fn prepare(&self, call: &ToolCall, rejected: Option<Rejection>) -> Prepared {
        match self.prepared_call(call, rejected) {
            Ok(prepared) => completed(prepared, &call.name),
            Err(rejection) => Prepared::Rejected(rejection),
        }
    }

    fn prepare_uncompleted(&self, call: &ToolCall, rejected: Option<Rejection>) -> Prepared {
        match self.prepared_call(call, rejected) {
            Ok(prepared) => inspected(prepared, &call.name),
            Err(rejection) => Prepared::Rejected(rejection),
        }
    }

    fn prepared_call(
        &self,
        call: &ToolCall,
        rejected: Option<Rejection>,
    ) -> Result<Box<dyn PreparedCall>, Rejection> {
        if let Some(rejection) = rejected {
            return Err(rejection);
        }
        if let Some(output) = self
            .mode
            .as_ref()
            .and_then(|mode| denial(mode, &self.tool_specs[..self.tools.len()], &call.name))
        {
            return Err(Rejection {
                reason: ToolRejection::Invalid,
                description: None,
                output: Box::new(output),
            });
        }
        let Some(tool) = self.tool(&call.name) else {
            if self
                .dynamic
                .as_ref()
                .is_some_and(|set| set.lists(&call.name))
            {
                return Err(Rejection::not_selected(&call.name));
            }
            return Err(Rejection {
                reason: ToolRejection::Unsupported,
                description: None,
                output: Box::new(ToolOutput::failure(format!(
                    "Unsupported tool: {}",
                    call.name
                ))),
            });
        };
        match contained(|| tool.prepare(&call.arguments)) {
            Some(Ok(prepared)) => Ok(prepared),
            Some(Err(output)) => Err(Rejection {
                reason: ToolRejection::Invalid,
                description: None,
                output: Box::new(output),
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
            turn_trace::continuation_injected(
                turn.silent_tool_steps,
                completion.content.as_deref(),
                completion.provider_replay.is_some(),
            );
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
        let presented = match &turn.stop.candidate {
            Some(candidate) => join_visible_segments(candidate, &history_text),
            None => history_text,
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
        Ok(Some(presented))
    }

    fn forget_summary_prompt(&mut self, turn: &Turn) {
        if !turn.summary_requested {
            return;
        }
        let start = (turn.start + 1).min(self.history.len());
        let prompt = self.history[start..].iter().rposition(|message| {
            matches!(
                message,
                ChatMessage::User {
                    content,
                    restored_steering: false,
                    feedback_for: None,
                    ..
                } if content == SUMMARIZE_PROMPT
            )
        });
        if let Some(index) = prompt {
            self.history.remove(start + index);
        }
    }

    fn has_turn_progress(&self, start: usize) -> bool {
        self.history[start + 1..]
            .iter()
            .any(|message| match message {
                ChatMessage::Tool { .. } => true,
                ChatMessage::User {
                    content,
                    restored_steering,
                    feedback_for,
                    ..
                } => {
                    *restored_steering || feedback_for.is_some() || steering_text(content).is_some()
                }
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

fn presentation_outcome(outcome: TurnOutcome, ending: Ending) -> TurnPresentationOutcome {
    match (outcome, ending) {
        (_, Ending::Paused) => TurnPresentationOutcome::Paused,
        (TurnOutcome::Completed, _) => TurnPresentationOutcome::Completed,
        (TurnOutcome::Interrupted, _) => TurnPresentationOutcome::Interrupted,
        (TurnOutcome::Failed, _) => TurnPresentationOutcome::Failed,
    }
}

fn prepared_for_history(
    call: ToolCall,
    malformed: Option<ToolOutput>,
    hooked: ToolPreparation,
) -> (ToolCall, Option<Rejection>) {
    if let Some(output) = malformed {
        let call = ToolCall {
            arguments: REPLAYED_MALFORMED_ARGUMENTS.to_owned(),
            ..call
        };
        let rejection = Rejection {
            reason: ToolRejection::MalformedArguments,
            description: None,
            output: Box::new(output),
        };
        return (call, Some(rejection));
    }
    match hooked {
        ToolPreparation::Unchanged => (call, None),
        ToolPreparation::Rewritten(arguments) => (ToolCall { arguments, ..call }, None),
        ToolPreparation::Blocked(content) => {
            let rejection = Rejection {
                reason: ToolRejection::Invalid,
                description: None,
                output: Box::new(ToolOutput::failure(content)),
            };
            (call, Some(rejection))
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
    action: ModelRecoveryAction,
    decision: &Decision,
    error: &ProviderError,
) -> RouteRecoveryStatus {
    RouteRecoveryStatus {
        kind: RouteRecoveryKind::AutoRetry,
        failed_attempt: attempt,
        succeeded_attempt: 0,
        attempt_limit: DEFAULT_MAX_PROVIDER_ATTEMPTS,
        cause: Some(cause),
        action: Some(action),
        required_action: ModelRecoveryRequiredAction::None,
        delay_seconds: decision.delay.as_secs(),
        diagnostic: Some(failure_diagnostic(error)),
        retry_wait: Some(decision.delay),
    }
}

struct Attempt {
    streamed: Result<Completion, ProviderError>,
    partial: String,
    streamed_bytes: usize,
    admitted: bool,
    tool: ToolEvidence,
    settled: Result<(), LogFailure>,
}

fn stopped_status(
    cause: Option<ModelRecoveryCause>,
    attempt: usize,
    consumed: usize,
    error: &ProviderError,
    spoke: bool,
) -> Option<RouteRecoveryStatus> {
    let unanswered = attempt > 1 && error.status.is_none();
    (!spoke && unanswered).then(|| RouteRecoveryStatus {
        kind: RouteRecoveryKind::TerminalProviderError,
        failed_attempt: consumed,
        succeeded_attempt: 0,
        attempt_limit: DEFAULT_MAX_PROVIDER_ATTEMPTS,
        cause,
        action: None,
        required_action: ModelRecoveryRequiredAction::None,
        delay_seconds: 0,
        diagnostic: Some(failure_diagnostic(error)),
        retry_wait: None,
    })
}

fn stalled_status(
    cause: ModelRecoveryCause,
    consumed: usize,
    error: &ProviderError,
    decision: &Decision,
) -> RouteRecoveryStatus {
    RouteRecoveryStatus {
        kind: RouteRecoveryKind::TerminalProviderError,
        failed_attempt: consumed,
        succeeded_attempt: 0,
        attempt_limit: DEFAULT_MAX_PROVIDER_ATTEMPTS,
        cause: Some(cause),
        action: None,
        required_action: decision.required_action,
        delay_seconds: 0,
        diagnostic: Some(failure_diagnostic(error)),
        retry_wait: None,
    }
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
        required_action: ModelRecoveryRequiredAction::None,
        delay_seconds: 0,
        diagnostic: None,
        retry_wait: None,
    }
}

struct Rejection {
    reason: ToolRejection,
    description: Option<Box<CallDescription>>,
    output: Box<ToolOutput>,
}

impl Rejection {
    fn not_selected(tool_name: &str) -> Self {
        Self {
            reason: ToolRejection::Invalid,
            description: Some(Box::new(CallDescription {
                title: format_unknown_action(tool_name),
                label: None,
                activity: ToolActivity::Command,
                effect: ToolEffect::None,
                concurrency: Concurrency::Serial,
            })),
            output: Box::new(ToolOutput::failure(not_selected(tool_name))),
        }
    }

    fn panicked(tool_name: &str) -> Self {
        Self {
            reason: ToolRejection::Panicked,
            description: None,
            output: Box::new(panicked(tool_name)),
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
            output: Box::new(output),
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

impl ParallelGroup {
    const fn name(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Subagent => "subagent",
        }
    }
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
    Admitted(Box<dyn PreparedCall>, ToolContext),
    Running(JoinHandle<Finished>, i64),
    Unstarted,
}

type Finished = (ToolOutput, i64, u64);

impl Dispatched {
    fn start(self, cancel: &CancellationToken, finishes: &Arc<AtomicU64>) -> Self {
        match self {
            Self::Admitted(prepared, _) if cancel.is_cancelled() => {
                discard(prepared);
                Self::Unstarted
            }
            Self::Admitted(prepared, context) => {
                let finishes = Arc::clone(finishes);
                Self::Running(
                    tokio::spawn(async move {
                        let output = prepared.execute(context).await;
                        let order = finishes.fetch_add(1, Ordering::SeqCst);
                        (output, ofx_trace::timestamp_ms(), order)
                    }),
                    ofx_trace::timestamp_ms(),
                )
            }
            other => other,
        }
    }
}

#[derive(Clone, Copy)]
struct Grace {
    deadline: Instant,
    cancelled_at: u64,
}

struct Settled<'c> {
    call: &'c ToolCall,
    output: Option<ToolOutput>,
    escalates: bool,
    executed: bool,
    review_hold: bool,
    feedback: Option<String>,
    ran: Option<Ran>,
}

#[derive(Clone, Copy)]
struct Ran {
    started_at_ms: i64,
    finished_at_ms: i64,
    panicked: bool,
    cancelled: bool,
}

struct Joined {
    output: ToolOutput,
    finished_at_ms: i64,
    panicked: bool,
    cancelled: bool,
}

struct SettledGroup<'c> {
    outcomes: Vec<Settled<'c>>,
    blocked: Option<BlockedCall>,
    stopped_at: Option<&'c ToolCall>,
}

#[derive(Clone, Copy)]
struct Gate<'a> {
    permissions: &'a dyn PermissionGate,
    approvals: Option<&'a Approvals>,
    reviews_fall_back_to_approval: bool,
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
) -> (Verdict, Option<String>) {
    let verdict = match admission {
        Admission::Allowed(path_access) => Verdict::Run(path_access),
        Admission::ApprovalRequired => {
            return ask_approval(gate, turn_id, &judged, events, cancel).await;
        }
        Admission::ReviewRequired => {
            let Some(verdict) = review(gate, reviewing, &judged, cancel).await else {
                return (Verdict::Interrupted, None);
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
                _ if gate.approvals.is_some() && gate.reviews_fall_back_to_approval => {
                    return ask_approval(gate, turn_id, &judged, events, cancel).await;
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
    };
    (verdict, None)
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
) -> (Verdict, Option<String>) {
    let Some(approvals) = gate.approvals else {
        return (Verdict::Blocked, None);
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
        answer = pending.answer() => Some(answer),
    };
    let decision = answer.as_ref().map(|answer| answer.decision);
    if let (Some(ApprovalDecision::Always), Some(grant)) = (decision, &scope.always) {
        gate.permissions.remember_approval(grant);
    }
    let verdict = match decision {
        _ if cancel.is_cancelled() => Verdict::Interrupted,
        None | Some(ApprovalDecision::Deny) => Verdict::Denied,
        Some(ApprovalDecision::Once | ApprovalDecision::Always) => Verdict::Run(scope.access),
    };
    let feedback = answer
        .and_then(|answer| answer.feedback)
        .filter(|text| !text.is_empty());
    (verdict, feedback)
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
    (turn_id, trace, parallel): (TurnId, TraceContext, Option<ParallelGroup>),
    group: Vec<(&'c ToolCall, Prepared)>,
    gate: Gate<'_>,
    reviewing: &mut Reviewing<'_>,
    events: EventSink<'_>,
    cancel: &CancellationToken,
) -> SettledGroup<'c> {
    let mut dispatched = Vec::with_capacity(group.len());
    let (statuses, mut reported) = unbounded_channel();
    let mut blocked = None;
    let mut stopped_at = None;
    let mut group = group.into_iter();
    for (call, prepared) in group.by_ref() {
        turn_trace::tool_call(trace, call);
        if cancel.is_cancelled() {
            discard(prepared);
            stopped_at = Some(call);
            break;
        }
        match prepared {
            Prepared::Rejected(Rejection {
                reason,
                description,
                output,
            }) => {
                events(tool_rejected(turn_id, call, reason, description, &output));
                dispatched.push((call, Dispatched::Rejected(*output, reason), None));
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
                let (verdict, feedback) = judge(
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
                    blocked = Some(blocked_call(call, &description));
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
                        dispatched.push((call, Dispatched::Admitted(prepared, context), feedback));
                        continue;
                    }
                    Verdict::Held(output) => (output, true),
                    Verdict::Denied => (tool_permission_denied_json(&call.name), false),
                    Verdict::Blocked | Verdict::Interrupted => {
                        if shown_while_reviewed {
                            events(tool_finished(turn_id, call, None));
                        }
                        stopped_at = (verdict == Verdict::Interrupted).then_some(call);
                        discard(prepared);
                        break;
                    }
                };
                discard(prepared);
                dispatched.push((
                    call,
                    Dispatched::Held(ToolOutput::failure(held), review_hold),
                    feedback,
                ));
            }
        }
    }
    group.for_each(discard);
    drop(statuses);
    let traced = (turn_id, trace, parallel);
    let outcomes = settle_group(traced, dispatched, &mut reported, events, cancel).await;
    SettledGroup {
        outcomes,
        blocked,
        stopped_at,
    }
}

async fn settle_group<'c>(
    (turn_id, trace, parallel): (TurnId, TraceContext, Option<ParallelGroup>),
    dispatched: Vec<(&'c ToolCall, Dispatched, Option<String>)>,
    reported: &mut UnboundedReceiver<ChildStatus>,
    events: EventSink<'_>,
    cancel: &CancellationToken,
) -> Vec<Settled<'c>> {
    let finishes = Arc::new(AtomicU64::new(0));
    let dispatched: Vec<_> = dispatched
        .into_iter()
        .map(|(call, dispatched, feedback)| {
            let started = dispatched.start(cancel, &finishes);
            if matches!(started, Dispatched::Running(..)) {
                turn_trace::tool_execution_start(trace, call);
            }
            (call, started, feedback)
        })
        .collect();
    let running = dispatched
        .iter()
        .filter(|(_, dispatched, _)| matches!(dispatched, Dispatched::Running(..)))
        .count();
    if let Some(kind) = parallel.filter(|_| running > 0) {
        turn_trace::parallel_group(trace, "parallel_tool_group_start", kind.name(), running);
    }
    let mut grace = None;
    let mut outcomes = Vec::with_capacity(dispatched.len());
    for (call, dispatched, feedback) in dispatched {
        let mut ran = None;
        let (output, escalates, executed, review_hold) = match dispatched {
            Dispatched::Rejected(output, reason) => {
                report_context_notices(turn_id, &output, events);
                (
                    Some(output),
                    reason != ToolRejection::MalformedArguments,
                    false,
                    false,
                )
            }
            Dispatched::Held(output, review_hold) => {
                events(tool_finished(turn_id, call, Some(&output)));
                (Some(output), true, false, review_hold)
            }
            Dispatched::Running(mut task, started_at_ms) => {
                let settling = settle(call, &mut task, (cancel, &finishes), &mut grace);
                let joined = forward_child_statuses(turn_id, settling, reported, events).await;
                ran = Some(Ran {
                    started_at_ms,
                    finished_at_ms: joined
                        .as_ref()
                        .map_or_else(ofx_trace::timestamp_ms, |joined| joined.finished_at_ms),
                    panicked: joined.as_ref().is_some_and(|joined| joined.panicked),
                    cancelled: joined.as_ref().is_none_or(|joined| joined.cancelled),
                });
                let output = joined.map(|joined| joined.output);
                if let Some(output) = &output {
                    report_context_notices(turn_id, output, events);
                }
                events(tool_finished(turn_id, call, output.as_ref()));
                (output, true, true, false)
            }
            Dispatched::Admitted(prepared, _) => {
                discard(prepared);
                events(tool_finished(turn_id, call, None));
                (None, true, false, false)
            }
            Dispatched::Unstarted => {
                events(tool_finished(turn_id, call, None));
                (None, true, false, false)
            }
        };
        if let Some(text) = &feedback {
            events(UiEvent::ApprovalFeedback {
                turn_id,
                text: text.clone(),
            });
        }
        outcomes.push(Settled {
            call,
            output,
            escalates,
            executed,
            review_hold,
            feedback,
            ran,
        });
    }
    outcomes
}

fn blocked_call(call: &ToolCall, description: &CallDescription) -> BlockedCall {
    BlockedCall {
        tool_name: call.name.clone(),
        arguments: call.arguments.clone(),
        title: description.title.clone(),
    }
}

fn enter_step(turn: &mut Turn, step: u64) -> bool {
    let entered = turn.trail.enter_step(step);
    if entered {
        turn.trace.step_id = ofx_trace::next_step_id();
    }
    entered
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Finish {
    Reply,
    ProviderResults,
    Batch,
}

fn settled_finish(turn: &Turn, completion: &Completion) -> Result<Finish, Stop> {
    let finish = match (completion.finish_reason, completion.tool_calls.is_empty()) {
        (FinishReason::Stop, true) => Some(Finish::Reply),
        (FinishReason::Stop, false) if ends_with_provider_results(completion) => {
            Some(Finish::ProviderResults)
        }
        (FinishReason::Stop, false) if completion.tool_calls.iter().all(provider_executed) => {
            Some(Finish::Batch)
        }
        (FinishReason::ToolCalls, false) => Some(Finish::Batch),
        _ => None,
    };
    let Some(finish) = finish else {
        turn_trace::invalid_tool_finish(turn.trace, completion);
        return Err(Stop::failed(TurnFailure::InvalidCompletion));
    };
    if let Some(failure) = malformed_provider_calls(&completion.tool_calls) {
        turn_trace::malformed_provider_call(turn.trace, &failure);
        return Err(Stop::failed(failure));
    }
    Ok(finish)
}

fn ran_outcome(output: &ToolOutput, panicked: bool) -> ToolCallOutcome {
    match output.status {
        ToolResultStatus::Success => ToolCallOutcome::Succeeded,
        ToolResultStatus::Failure if panicked => ToolCallOutcome::RuntimeFailed,
        ToolResultStatus::Failure if output.command_result.is_some() => {
            ToolCallOutcome::CommandFailed
        }
        ToolResultStatus::Failure => ToolCallOutcome::ToolFailed,
    }
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
    task: &mut JoinHandle<Finished>,
    (cancel, finishes): (&CancellationToken, &AtomicU64),
    grace: &mut Option<Grace>,
) -> Option<Joined> {
    let current = if let Some(current) = *grace {
        current
    } else {
        tokio::select! {
            biased;
            joined = &mut *task => {
                return Some(settled_output(call, joined, None));
            }
            () = cancel.cancelled() => {}
        }
        *grace.insert(Grace {
            deadline: Instant::now() + TOOL_CANCEL_GRACE,
            cancelled_at: finishes.fetch_add(1, Ordering::SeqCst),
        })
    };
    if let Ok(joined) = tokio::time::timeout_at(current.deadline, &mut *task).await {
        return Some(settled_output(call, joined, Some(current.cancelled_at)));
    }
    task.abort();
    None
}

fn settled_output(
    call: &ToolCall,
    joined: Result<Finished, JoinError>,
    cancelled_at: Option<u64>,
) -> Joined {
    match joined {
        Ok((output, finished_at_ms, finished)) => Joined {
            output,
            finished_at_ms,
            panicked: false,
            cancelled: cancelled_at.is_some_and(|cancelled_at| finished > cancelled_at),
        },
        Err(error) => {
            if let Ok(payload) = error.try_into_panic() {
                release(payload);
            }
            Joined {
                output: panicked(&call.name),
                finished_at_ms: ofx_trace::timestamp_ms(),
                panicked: true,
                cancelled: cancelled_at.is_some(),
            }
        }
    }
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
