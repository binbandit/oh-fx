mod failures;

use std::path::Path;

use failures::{
    attach_discovery_notice, execute_primary_budget, format_ambiguous_skill,
    format_exact_skill_not_found, format_missing_skill, format_skill_location_mismatch,
};

use crate::skill_contract::{
    CallPreparation, ExecuteOutput, PreparedSkill, Skill, SkillDiagnostic,
};
use crate::skill_runtime::{SkillResolution, diagnostic_summary, find_skill_at, resolve_skill};

#[derive(Debug, Clone, Copy)]
pub struct SkillInventory<'a> {
    pub skills: &'a [Skill],
    pub diagnostics: &'a [SkillDiagnostic],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SkillError {
    #[error("InvalidSkillLocation")]
    InvalidSkillLocation,
}

pub fn prepare_identity(
    inventory: &SkillInventory<'_>,
    name: Option<&str>,
    location: Option<&Path>,
    max_tool_result_bytes: usize,
) -> Result<CallPreparation, SkillError> {
    let resolution = match (name, location) {
        (Some(name), _) => resolve_skill(inventory.skills, name, location),
        (None, Some(location)) => find_skill_at(inventory.skills, location)
            .map_or(SkillResolution::NotFound, SkillResolution::Found),
        (None, None) => return Err(SkillError::InvalidSkillLocation),
    };
    if let SkillResolution::Found(skill) = resolution {
        return Ok(CallPreparation::Selected(PreparedSkill {
            skill: skill.clone(),
            diagnostics: inventory.diagnostics.to_vec(),
        }));
    }
    let notice = diagnostic_summary(inventory.diagnostics);
    let budget = execute_primary_budget(max_tool_result_bytes, notice.is_some());
    let name = name.unwrap_or_default();
    let model_output = match (resolution, location) {
        (SkillResolution::AmbiguousName, _) => {
            format_ambiguous_skill(inventory.skills, name, budget)
        }
        (SkillResolution::NameLocationMismatch, Some(location)) => {
            format_skill_location_mismatch(name, location, budget)
        }
        (_, Some(location)) => format_exact_skill_not_found(name, location, budget),
        (_, None) => format_missing_skill(name, budget),
    };
    let failure = ExecuteOutput {
        model_output,
        ..ExecuteOutput::default()
    };
    Ok(CallPreparation::Failure(attach_discovery_notice(
        failure,
        notice,
        Some(max_tool_result_bytes),
    )))
}

#[cfg(test)]
mod tests;
