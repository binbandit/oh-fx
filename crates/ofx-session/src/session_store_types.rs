use crate::session_codec::SESSION_METADATA_SCHEMA_VERSION;

const SCHEMA_V3_VERSION: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorIssueKind {
    AuthorityTransitionPending,
    CanonicalStateInvalid,
    UnsafePath,
}

impl DoctorIssueKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::AuthorityTransitionPending => "authority_transition_pending",
            Self::CanonicalStateInvalid => "canonical_state_invalid",
            Self::UnsafePath => "unsafe_path",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorDiagnostic {
    pub session_id: String,
    pub kind: DoctorIssueKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorInspection {
    pub diagnostics: Vec<DoctorDiagnostic>,
    pub inspected_count: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMigrationStatus {
    Migrated,
    AlreadyCurrent,
}

impl SessionMigrationStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Migrated => "migrated",
            Self::AlreadyCurrent => "already_current",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMigration {
    pub session_id: String,
    pub source_schema_version: u8,
    pub source_bytes: u64,
    pub status: SessionMigrationStatus,
}

impl SessionMigration {
    pub(crate) fn already_current(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_owned(),
            source_schema_version: SESSION_METADATA_SCHEMA_VERSION,
            source_bytes: 0,
            status: SessionMigrationStatus::AlreadyCurrent,
        }
    }

    pub(crate) fn migrated(session_id: &str, source_bytes: u64) -> Self {
        Self {
            session_id: session_id.to_owned(),
            source_schema_version: SCHEMA_V3_VERSION,
            source_bytes,
            status: SessionMigrationStatus::Migrated,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRecoveryStatus {
    Recovered,
    RecoveredWithUnverifiedArtifacts,
    Indeterminate,
}

impl SessionRecoveryStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Recovered => "recovered",
            Self::RecoveredWithUnverifiedArtifacts => "recovered_with_unverified_artifacts",
            Self::Indeterminate => "indeterminate",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecovery {
    pub source_session_id: String,
    pub recovered_session_id: String,
    pub history_len: usize,
    pub usage_incomplete: bool,
    pub status: SessionRecoveryStatus,
}
