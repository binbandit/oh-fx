use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::applicable_target::ApplicableTarget;
use crate::auto_classifier::ReviewFailure;
use crate::ids::ToolCallId;
use crate::stream_provider::BoxFuture;
use crate::types::{ChatMessage, ToolCall, Usage};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathAccess {
    WorkspaceOnly,
    WorkspaceOrExternal,
    Within(PathBuf),
}

impl PathAccess {
    pub fn confining_root<'a>(&'a self, workspace_root: &'a Path) -> Option<&'a Path> {
        match self {
            Self::WorkspaceOnly => Some(workspace_root),
            Self::WorkspaceOrExternal => None,
            Self::Within(root) => Some(root),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Admission {
    Allowed(PathAccess),
    ApprovalRequired,
    ReviewRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileMutationState {
    Creates,
    Changes,
    Unchanged,
    Unread,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileMutation {
    pub target: PathBuf,
    pub state: FileMutationState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandProfile {
    Clean,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CommandRequest {
    Run {
        command: String,
        cwd: PathBuf,
        profile: CommandProfile,
        shell: Option<PathBuf>,
        terminal: bool,
    },
    Observe,
    SendInput {
        input: String,
    },
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatedAction<'a> {
    Call(&'a ToolCall),
    FileMutation(&'a FileMutation),
    Command(&'a CommandRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SessionGrant {
    WorkspaceFiles,
    FileChangesUnder(PathBuf),
    ReadsUnder(PathBuf),
    GlobsUnder(PathBuf),
    GrepsUnder(PathBuf),
    Command {
        command: String,
        cwd: PathBuf,
        profile: CommandProfile,
        shell: Option<PathBuf>,
        terminal: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApprovalScope {
    pub target: Option<PathBuf>,
    pub access: PathAccess,
    pub always: Option<SessionGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub display_path: String,
    pub before: Option<Vec<u8>>,
    pub after: Vec<u8>,
    pub parents: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ReviewRequest<'a> {
    pub model: &'a str,
    pub current_request: &'a str,
    pub earlier_requests: &'a [&'a str],
    pub compacted_turns: Option<usize>,
    pub turn: &'a [ChatMessage],
    pub held: &'a [(ToolCallId, String)],
    pub batch: &'a [ToolCall],
    pub call: &'a ToolCall,
    pub action: GatedAction<'a>,
    pub file: Option<&'a FileChange>,
    pub attempt_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewVerdict {
    Clear,
    Caution(String),
    EvidenceIncomplete,
    Unavailable(ReviewFailure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reviewed {
    pub verdict: ReviewVerdict,
    pub usage: Usage,
}

impl Reviewed {
    pub fn unavailable(failure: ReviewFailure) -> Self {
        Self {
            verdict: ReviewVerdict::Unavailable(failure),
            usage: Usage::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApprovalDecision {
    Once,
    Always,
    Deny,
}

pub trait PermissionGate: Send + Sync {
    fn admit(&self, call: &ToolCall) -> Admission;

    fn applicable_target(&self, call: &ToolCall) -> Option<ApplicableTarget>;

    fn admit_file_mutation(&self, _mutation: &FileMutation) -> Admission {
        Admission::ApprovalRequired
    }

    fn admit_command(&self, _request: &CommandRequest) -> Admission {
        Admission::ApprovalRequired
    }

    fn approval_scope(&self, _action: GatedAction<'_>) -> ApprovalScope {
        ApprovalScope {
            target: None,
            access: PathAccess::WorkspaceOrExternal,
            always: None,
        }
    }

    fn remember_approval(&self, _grant: &SessionGrant) {}

    fn forget_approvals(&self);

    fn review<'a>(
        &'a self,
        _request: ReviewRequest<'a>,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Option<Reviewed>> {
        Box::pin(async { Some(Reviewed::unavailable(ReviewFailure::ReviewerUnconfigured)) })
    }
}
