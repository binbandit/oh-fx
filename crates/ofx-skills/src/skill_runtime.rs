mod candidate;
mod catalog;
mod diagnostics;
mod discovery;
mod explicit;
mod resolution;
mod skill_file;
mod symlink_authorities;

pub(crate) use candidate::{
    CandidateOpen, OpenedSkillCandidate, ResourceOpenError, open_validated_skill_candidate,
    resource_is_skill_file,
};
pub use catalog::{SkillCatalog, build_skill_prompt};
pub(crate) use diagnostics::diagnostic_summary;
pub use discovery::{SkillDiscovery, SkillDiscoveryContext};
pub(crate) use explicit::{
    ExplicitSelection, collect_explicit_skill_selections, explicit_name_candidates,
};
pub(crate) use resolution::{SkillResolution, find_skill_at, resolve_skill, skills_named};
pub use symlink_authorities::SymlinkAuthorities;
