mod metadata;
mod metadata_prefix;

use std::path::PathBuf;

pub use metadata::InvalidMetadataCause;
pub(crate) use metadata::{SkillMetadata, parse_skill_file, resolve_metadata};
pub(crate) use metadata_prefix::{MetadataPrefixError, read_metadata_prefix};

pub(crate) const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;
pub(crate) const MAX_NAME_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub source: SkillSource,
    pub read_authority: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillSource {
    WorkspaceOhFx,
    WorkspaceShared,
    WorkspaceOpencode,
    WorkspaceCodex,
    WorkspaceClaude,
    WorkspaceAgents,
    WorkspaceClaw,
    GlobalOhFx,
    GlobalOpencode,
    GlobalCodex,
    GlobalClaude,
    GlobalAgents,
    GlobalClaw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDiagnosticCause {
    InvalidMetadata(InvalidMetadataCause),
    LinkedCandidateUnavailable,
    Unreadable,
    Oversized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDiagnosticScope {
    Root,
    Candidate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub source: SkillSource,
    pub scope: SkillDiagnosticScope,
    pub cause: SkillDiagnosticCause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootSpec {
    pub source: SkillSource,
    pub path: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootPolicy {
    pub workspace_roots: &'static [RootSpec],
    pub managed_root_source: Option<SkillSource>,
    pub global_roots: &'static [RootSpec],
}
