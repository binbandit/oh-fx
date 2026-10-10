use std::io;

use ofx_config::DurableError;

use crate::session_usage::UsageSnapshotError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    #[error("InvalidSessionId")]
    InvalidSessionId,
    #[error("InvalidWorkspaceRoot")]
    InvalidWorkspaceRoot,
    #[error("InvalidDurableField")]
    InvalidDurableField,
    #[error("SessionMetadataTooLarge")]
    SessionMetadataTooLarge,
    #[error("InvalidSessionMetadata")]
    InvalidSessionMetadata,
    #[error("UnsupportedSessionSchema")]
    UnsupportedSessionSchema,
    #[error("InvalidSessionFormat")]
    InvalidSessionFormat,
    #[error("InvalidConversationFrame")]
    InvalidConversationFrame,
    #[error("InvalidConversationEvent")]
    InvalidConversationEvent,
    #[error("OutOfOrderConversationEvent")]
    OutOfOrderConversationEvent,
    #[error("DuplicateToolCall")]
    DuplicateToolCall,
    #[error("OrphanToolResult")]
    OrphanToolResult,
    #[error("ToolIdentityMismatch")]
    ToolIdentityMismatch,
    #[error("InvalidCheckpointCoverage")]
    InvalidCheckpointCoverage,
    #[error("InvalidRecoveryCheckpoint")]
    InvalidRecoveryCheckpoint,
    #[error("NoPendingRecovery")]
    NoPendingRecovery,
    #[error("RecoveryCredentialAuthorityChanged")]
    RecoveryCredentialAuthorityChanged,
    #[error("InvalidContextHistoryStart")]
    InvalidContextHistoryStart,
    #[error("UnresolvedToolCall")]
    UnresolvedToolCall,
    #[error("EventFrameTooLarge")]
    EventFrameTooLarge,
    #[error("ConversationSequenceOverflow")]
    ConversationSequenceOverflow,
    #[error("SessionNotFound")]
    SessionNotFound,
    #[error("SessionAlreadyExists")]
    SessionAlreadyExists,
    #[error("SessionStartFailed")]
    SessionStartFailed,
    #[error("SessionBusy")]
    SessionBusy,
    #[error("SessionLockUnsupported")]
    SessionLockUnsupported,
    #[error("SessionPathUnsafe")]
    SessionPathUnsafe,
    #[error("PrivateStatePermissionsUnsupported")]
    PrivateStatePermissionsUnsupported,
    #[error("DurableLayoutFailed")]
    DurableLayoutFailed,
    #[error("SessionStoreUnavailable")]
    SessionStoreUnavailable,
    #[error("SessionStoreReadOnly")]
    SessionStoreReadOnly,
    #[error("SessionWriterChanged")]
    SessionWriterChanged,
    #[error("SessionPersistenceUncertain")]
    SessionPersistenceUncertain,
    #[error("SessionCommitFailed")]
    SessionCommitFailed,
    #[error("SessionTargetChanged")]
    SessionTargetChanged,
    #[error("InvalidRememberedSession")]
    InvalidRememberedSession,
    #[error("NoSavedSessions")]
    NoSavedSessions,
    #[error("NoReadableSessions")]
    NoReadableSessions,
    #[error("OneOffSessionNotResumable")]
    OneOffSessionNotResumable,
    #[error("FxSessionOpen")]
    FxSessionOpen,
    #[error("FxCompactionUnfinished")]
    FxCompactionUnfinished,
    #[error("FxSessionUnreadable")]
    FxSessionUnreadable,
    #[error("InvalidUsageSidecar")]
    InvalidUsageSidecar,
    #[error("UsageSidecarTooLarge")]
    UsageSidecarTooLarge,
    #[error("InvalidUsageSnapshot")]
    InvalidUsageSnapshot,
    #[error("UsageCapacityExceeded")]
    UsageCapacityExceeded,
    #[error("UsageSidecarSessionMismatch")]
    UsageSidecarSessionMismatch,
    #[error("UnsupportedUsageSidecar")]
    UnsupportedUsageSidecar,
    #[error("SessionRecoveryNotNeeded")]
    SessionRecoveryNotNeeded,
    #[error("SessionRecoveryRequiresCurrentSchema")]
    SessionRecoveryRequiresCurrentSchema,
    #[error("SessionRecoveryBoundaryInvalid")]
    SessionRecoveryBoundaryInvalid,
    #[error("SessionRecoveryIndeterminate")]
    SessionRecoveryIndeterminate,
    #[error(transparent)]
    Storage(DurableError),
    #[error("{0:?}")]
    Io(io::ErrorKind),
}

impl From<DurableError> for SessionError {
    fn from(error: DurableError) -> Self {
        match error {
            DurableError::PathUnsafe => Self::SessionPathUnsafe,
            DurableError::PermissionsUnsupported => Self::PrivateStatePermissionsUnsupported,
            DurableError::LockUnsupported => Self::SessionLockUnsupported,
            other => Self::Storage(other),
        }
    }
}

impl From<UsageSnapshotError> for SessionError {
    fn from(error: UsageSnapshotError) -> Self {
        match error {
            UsageSnapshotError::Invalid => Self::InvalidUsageSnapshot,
            UsageSnapshotError::CapacityExceeded => Self::UsageCapacityExceeded,
        }
    }
}

impl From<io::Error> for SessionError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

impl From<rustix::io::Errno> for SessionError {
    fn from(errno: rustix::io::Errno) -> Self {
        io::Error::from(errno).into()
    }
}
