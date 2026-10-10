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
