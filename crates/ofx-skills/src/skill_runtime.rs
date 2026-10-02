mod catalog;
mod diagnostics;
mod discovery;
mod explicit;
mod resolution;
mod skill_file;
mod symlink_authorities;

pub use catalog::{SkillCatalog, build_skill_prompt};
pub(crate) use diagnostics::diagnostic_summary;
pub use discovery::{SkillDiscovery, SkillDiscoveryContext};
pub use explicit::{ExplicitSelection, collect_explicit_skill_selections};
pub(crate) use resolution::{SkillResolution, find_skill_at, resolve_skill};
pub use symlink_authorities::SymlinkAuthorities;
