mod byte_trim;
mod encoded_scalar;
mod file_picker_path;
mod install;
mod io;
mod skill_contract;
mod skill_invocation;
mod skill_runtime;
#[cfg(test)]
mod test_fixture;

pub use skill_contract::{
    CallPreparation, ExecuteOutput, InvalidMetadataCause, LocationError, Locations, PreparedSkill,
    RootPolicy, RootSpec, Skill, SkillDiagnostic, SkillDiagnosticCause, SkillDiagnosticScope,
    SkillSource,
};
pub use skill_invocation::{
    ExecuteResult, ExplicitBinding, ExplicitPromptSection, LoadNotice, NoticeTone, SkillError,
    SkillInventory, SkillLoader, prepare_identity,
};
pub use skill_runtime::{
    SkillCatalog, SkillDiscovery, SkillDiscoveryContext, SymlinkAuthorities, build_skill_prompt,
    diagnostic_summary,
};

pub use install::{InstallResult, install_local};
