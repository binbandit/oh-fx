use crate::ids::{RequestId, ToolCallId, TurnId};
use crate::model_capabilities::ModelCapabilities;
use crate::permission_gate::{
    ApprovalDecision, ApprovalScope, CommandRequest, FileMutation, ProposedFileChange,
};
use crate::session_picker::{ResumeRefusal, SessionCursor, SessionPage, SessionScope};
use crate::settings_catalog::{SettingId, SettingsSnapshot};
use crate::skill_menu::{SkillBinding, SkillMenuFocus, SkillMenuItem};
use crate::subagent::SubagentStatus;
use crate::tool_dispatch::CallDescription;
use crate::types::{
    CommandProcessPresentation, FileChangeStats, PermissionMode, QuestionBatchEntry,
    ReasoningEffort, RouteRecoveryStatus, ToolResultStatus, ToolStatusDetail, Usage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnOutcome {
    Completed,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRejection {
    Unsupported,
    MalformedArguments,
    Invalid,
    Panicked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDeferral {
    ProjectInstructions,
    TargetChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoticeTone {
    Information,
    Success,
    Warning,
    Error,
    Cancelled,
    Neutral,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeLink {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub topic: String,
    pub tone: NoticeTone,
    pub body: String,
    pub link: Option<NoticeLink>,
}

impl Notice {
    pub fn new(tone: NoticeTone, topic: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            topic: topic.into(),
            tone,
            body: body.into(),
            link: None,
        }
    }

    #[must_use]
    pub fn with_link(mut self, label: impl Into<String>, url: impl Into<String>) -> Self {
        self.link = Some(NoticeLink {
            label: label.into(),
            url: url.into(),
        });
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionEnd {
    NothingToCompact,
    Busy,
    Cancelled,
    Failed,
    ContextTooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionActivity {
    Preparing,
    Summarizing,
    Compacted,
    Ended(CompactionEnd),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatuslineItem {
    Context,
    Session,
    Workspace,
}

impl StatuslineItem {
    pub const ALL: [Self; 3] = [Self::Context, Self::Session, Self::Workspace];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::Session => "session",
            Self::Workspace => "workspace",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatuslineToggles {
    context: bool,
    session: bool,
    workspace: bool,
}

impl StatuslineToggles {
    pub const fn enabled(self, item: StatuslineItem) -> bool {
        match item {
            StatuslineItem::Context => self.context,
            StatuslineItem::Session => self.session,
            StatuslineItem::Workspace => self.workspace,
        }
    }

    pub fn set(&mut self, item: StatuslineItem, enabled: bool) {
        match item {
            StatuslineItem::Context => self.context = enabled,
            StatuslineItem::Session => self.session = enabled,
            StatuslineItem::Workspace => self.workspace = enabled,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    pub label: String,
    pub branch: Option<String>,
}

pub trait WorkspaceIdentitySource: Send {
    fn refresh(&mut self) -> WorkspaceIdentity;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryEntry {
    User(String),
    Assistant(String),
    QuestionsAnswered(Vec<(String, String)>),
    Cancelled,
    Notice(Notice),
    Tool(SavedToolCall),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedToolCall {
    pub call_id: ToolCallId,
    pub tool_name: String,
    pub arguments: String,
    pub description: Option<CallDescription>,
    pub status: ToolResultStatus,
    pub output: String,
    pub process: Option<CommandProcessPresentation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub id: RequestId,
    pub call_id: ToolCallId,
    pub tool_name: String,
    pub description: CallDescription,
    pub tool_arguments_preview: String,
    pub tool_arguments_truncated: bool,
    pub scope: ApprovalScope,
    pub command: Option<CommandRequest>,
    pub file: Option<FileMutation>,
    pub change: Option<ProposedFileChange>,
    pub origin: ApprovalOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalOrigin {
    ActiveSession,
    Subagent(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionRequest {
    pub id: RequestId,
    pub entries: Vec<QuestionBatchEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    pub id: String,
    pub capabilities: ModelCapabilities,
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCatalogSource {
    ProfileSettings,
    Subscription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogRetry {
    RateLimited,
    Unreachable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelCatalog {
    Listed {
        models: Vec<ModelOption>,
        source: ModelCatalogSource,
    },
    Failed {
        retry: Option<CatalogRetry>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    TurnStarted {
        turn_id: TurnId,
    },
    AssistantText {
        turn_id: TurnId,
        text: String,
    },
    AssistantRestarted {
        turn_id: TurnId,
        text: String,
    },
    ReasoningText {
        turn_id: TurnId,
        text: String,
    },
    Operational {
        turn_id: TurnId,
        text: String,
    },
    Recovery {
        turn_id: TurnId,
        status: RouteRecoveryStatus,
    },
    ToolStarted {
        turn_id: TurnId,
        call_id: ToolCallId,
        tool_name: String,
        description: CallDescription,
    },
    ToolFinished {
        turn_id: TurnId,
        call_id: ToolCallId,
        tool_name: String,
        arguments: String,
        status: ToolResultStatus,
        content: String,
        command_result: Option<String>,
        process: Option<CommandProcessPresentation>,
        status_detail: Option<ToolStatusDetail>,
        file_change: Option<FileChangeStats>,
    },
    SubagentStatus {
        turn_id: TurnId,
        call_id: ToolCallId,
        status: SubagentStatus,
    },
    ToolDeferred {
        turn_id: TurnId,
        call_id: ToolCallId,
        deferral: ToolDeferral,
    },
    ToolRejected {
        turn_id: TurnId,
        call_id: ToolCallId,
        tool_name: String,
        arguments: String,
        reason: ToolRejection,
        description: Option<CallDescription>,
        content: String,
    },
    ContextNotice {
        turn_id: TurnId,
        text: String,
    },
    SystemNotice {
        text: String,
    },
    SteeringApplied {
        turn_id: TurnId,
        prompt: u64,
        text: String,
    },
    ApprovalRequested {
        turn_id: TurnId,
        request: Box<ApprovalRequest>,
    },
    QuestionRequested {
        turn_id: TurnId,
        request: QuestionRequest,
    },
    UsageReported {
        turn_id: TurnId,
        usage: Usage,
        context_window: Option<u32>,
    },
    TurnFinished {
        turn_id: TurnId,
        outcome: TurnOutcome,
    },
    ApiStatus {
        turn_id: TurnId,
        text: String,
    },
    Notice {
        notice: Notice,
    },
    ModelSelected {
        model: String,
    },
    SessionTitleChanged {
        title: Option<String>,
    },
    ModelCatalog {
        provider: String,
        catalog: ModelCatalog,
    },
    ProviderPicker {
        prefix: String,
        providers: Vec<String>,
    },
    ProviderSelected {
        provider: String,
    },
    PermissionModeChanged {
        mode: PermissionMode,
        full_access_warning: bool,
    },
    StatuslineChanged {
        item: StatuslineItem,
        enabled: bool,
    },
    StatuslineMenuOpened,
    SettingsMenuOpened {
        snapshot: SettingsSnapshot,
    },
    SettingsChanged {
        snapshot: SettingsSnapshot,
    },
    PromptHistoryChanged {
        enabled: bool,
    },
    HelpRequested,
    StatsRequested,
    CompactionActivity {
        activity: CompactionActivity,
    },
    TurnCompaction {
        turn_id: TurnId,
        activity: CompactionActivity,
    },
    UpgradeStatus {
        label: String,
    },
    SkillsMenu {
        items: Vec<SkillMenuItem>,
        focus: SkillMenuFocus,
    },
    ConversationCleared {
        first_kept_prompt: u64,
    },
    SessionPickerOpened {
        scope: SessionScope,
    },
    SessionsListed {
        page: SessionPage,
    },
    SessionsUnavailable {
        scope: SessionScope,
    },
    SessionResumeFailed {
        id: String,
        refusal: ResumeRefusal,
    },
    SessionResumed {
        history: Vec<HistoryEntry>,
    },
    RecoveryContinuing {
        prompt: String,
        id: u64,
    },
    ExitRequested,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    Submit {
        prompt: String,
        skills: Vec<SkillBinding>,
    },
    ToggleStatusline {
        item: StatuslineItem,
    },
    SelectModelFromSettings {
        model: String,
    },
    StepSetting {
        setting: SettingId,
        delta: isize,
    },
    RunCommand {
        text: String,
    },
    ListModels,
    SelectProvider {
        provider: String,
    },
    SelectModel {
        model: String,
        effort: ReasoningEffort,
        fast_mode: Option<bool>,
    },
    Cancel {
        turn_id: TurnId,
    },
    PauseRecovery {
        turn_id: TurnId,
    },
    Approval {
        request_id: RequestId,
        decision: ApprovalDecision,
    },
    QuestionAnswered {
        request_id: RequestId,
        answers: Option<Vec<String>>,
    },
    TogglePermissionMode,
    FullAccessWarningShown,
    ApplyReadyUpgrade,
    CancelCompaction,
    OpenSessions {
        scope: SessionScope,
    },
    ListSessions {
        scope: SessionScope,
        after: Option<SessionCursor>,
        limit: usize,
    },
    ResumeSession {
        id: String,
    },
    CloseSessionPicker,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_carry_their_tone_topic_and_optional_link() {
        let plain = Notice::new(NoticeTone::Warning, "model", "unknown model");
        assert_eq!(plain.topic, "model");
        assert_eq!(plain.tone, NoticeTone::Warning);
        assert_eq!(plain.body, "unknown model");
        assert_eq!(plain.link, None);
        let linked =
            Notice::new(NoticeTone::Success, "", "updated").with_link("notes", "https://x");
        assert_eq!(
            linked.link,
            Some(NoticeLink {
                label: "notes".to_owned(),
                url: "https://x".to_owned(),
            })
        );
    }
}
