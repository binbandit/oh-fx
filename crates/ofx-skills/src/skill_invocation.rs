mod explicit_section;
mod failures;
mod resource;

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub use explicit_section::{ExplicitBinding, ExplicitPromptSection, LoadNotice, NoticeTone};
use failures::{
    attach_discovery_notice, bounded_skill_error, execute_primary_budget, format_ambiguous_skill,
    format_exact_skill_not_found, format_missing_skill, format_skill_location_mismatch,
    skill_chunk_blocked_marker, skill_chunk_notice, skill_chunk_truncated_marker,
    skill_file_blocked_marker, skill_file_blocked_notice,
};
use ofx_config::{
    ContextLimit, ContextLimitName, ContextLimitSource, ContextLimits, EMERGENCY_CEILING_BYTES,
    line_safe_prefix_length,
};
use ofx_text::{is_model_safe_text, sanitize_model_text_owned};
use ofx_workspace::PathError;
use resource::{
    SkillResourceRead, check_cancelled, read_skill_resource, revalidate_primary_identity,
    verify_read_identity,
};
use tokio_util::sync::CancellationToken;

use crate::encoded_scalar::{encoded_bytes, encoded_scalar};
use crate::skill_contract::{
    CallPreparation, ExecuteOutput, PreparedSkill, Skill, SkillDiagnostic, SkillDiagnosticScope,
    resource_path_or_main,
};
use crate::skill_runtime::{
    CandidateOpen, OpenedSkillCandidate, SkillResolution, SymlinkAuthorities, diagnostic_summary,
    find_skill_at, open_validated_skill_candidate, resolve_skill, resource_is_skill_file,
};

const OFFSET_BOUNDARY_FAILURE: &str =
    "skill offset must be at a valid UTF-8 boundary within the selected resource";
const SANITIZED_NOTICE: &str = "[context] Skill content was sanitized before delivery.\n";
const DEFAULT_LOCATION_FAILURE_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy)]
pub struct SkillInventory<'a> {
    pub skills: &'a [Skill],
    pub diagnostics: &'a [SkillDiagnostic],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SkillError {
    #[error("InvalidSkillLocation")]
    InvalidSkillLocation,
    #[error("Cancelled")]
    Cancelled,
    #[error("InvalidSkillResourcePath")]
    InvalidSkillResourcePath,
    #[error("InvalidSkillResource")]
    InvalidSkillResource,
    #[error("BinarySkillResource")]
    BinarySkillResource,
    #[error("SkillResourceChanged")]
    SkillResourceChanged,
    #[error("SkillFileLimitExceeded")]
    SkillFileLimitExceeded,
    #[error("UnexpectedEndOfFile")]
    UnexpectedEndOfFile,
    #[error("SkillContextTooLarge")]
    SkillContextTooLarge,
    #[error(transparent)]
    Path(#[from] PathError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecuteResult {
    Loaded(ExecuteOutput),
    Failure(ExecuteOutput),
}

impl ExecuteResult {
    pub fn output(&self) -> &ExecuteOutput {
        match self {
            Self::Loaded(output) | Self::Failure(output) => output,
        }
    }

    fn failure(model_output: String) -> Self {
        Self::Failure(ExecuteOutput {
            model_output,
            ..ExecuteOutput::default()
        })
    }

    fn context_limit_failure(model_output: String) -> Self {
        Self::Failure(ExecuteOutput {
            notice: Some(model_output.clone()),
            model_output,
            ..ExecuteOutput::default()
        })
    }
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

#[derive(Debug, Clone, Copy)]
struct LoadLimits {
    chunk: ContextLimit,
    file: ContextLimit,
}

pub struct SkillLoader<'a> {
    inventory: SkillInventory<'a>,
    authorities: &'a SymlinkAuthorities,
    limits: LoadLimits,
    max_tool_result_bytes: Option<usize>,
    cancellation: Option<&'a CancellationToken>,
    ceiling: usize,
    #[cfg(test)]
    after_selection: Option<&'a dyn Fn()>,
}

struct Selection<'s> {
    skill: &'s Skill,
    candidate: OpenedSkillCandidate,
    notice: Option<String>,
}

enum Selected<'s> {
    Ready(Selection<'s>),
    Failed(ExecuteResult),
}

impl<'a> SkillLoader<'a> {
    pub fn new(
        inventory: SkillInventory<'a>,
        authorities: &'a SymlinkAuthorities,
        limits: &ContextLimits,
    ) -> Self {
        Self {
            inventory,
            authorities,
            limits: LoadLimits {
                chunk: limits.get(ContextLimitName::SkillChunkBytes),
                file: limits.get(ContextLimitName::SkillFileBytes),
            },
            max_tool_result_bytes: None,
            cancellation: None,
            ceiling: EMERGENCY_CEILING_BYTES,
            #[cfg(test)]
            after_selection: None,
        }
    }

    #[must_use]
    pub fn with_max_tool_result_bytes(mut self, max_tool_result_bytes: usize) -> Self {
        self.max_tool_result_bytes = Some(max_tool_result_bytes);
        self
    }

    #[must_use]
    pub fn with_cancellation(mut self, cancellation: &'a CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    pub fn load_by_identity(
        &self,
        name: &str,
        location: Option<&Path>,
        resource: Option<&str>,
        offset: usize,
    ) -> Result<ExecuteResult, SkillError> {
        match self.select(name, location)? {
            Selected::Ready(selection) => {
                self.load_chunk(selection, resource_path_or_main(resource), offset)
            }
            Selected::Failed(failure) => Ok(failure),
        }
    }

    pub fn load_whole_by_location(
        &self,
        location: &Path,
        resource: Option<&str>,
    ) -> Result<ExecuteResult, SkillError> {
        let Some(skill) = find_skill_at(self.inventory.skills, location) else {
            let budget = self
                .max_tool_result_bytes
                .unwrap_or(DEFAULT_LOCATION_FAILURE_BYTES);
            return Ok(ExecuteResult::failure(format_exact_skill_not_found(
                "", location, budget,
            )));
        };
        match self.select(&skill.name, Some(location))? {
            Selected::Ready(selection) => {
                self.load_whole(selection, resource_path_or_main(resource))
            }
            Selected::Failed(failure) => Ok(failure),
        }
    }

    fn primary_budget(&self, notice: Option<&String>) -> usize {
        self.max_tool_result_bytes.map_or(usize::MAX, |limit| {
            execute_primary_budget(limit, notice.is_some())
        })
    }

    fn select(&self, name: &str, location: Option<&Path>) -> Result<Selected<'a>, SkillError> {
        check_cancelled(self.cancellation)?;
        let resolution = resolve_skill(self.inventory.skills, name, location);
        let validation = match resolution {
            SkillResolution::Found(skill) => {
                Some(open_validated_skill_candidate(skill, self.authorities))
            }
            _ => None,
        };
        let current_diagnostic = match (resolution, &validation) {
            (SkillResolution::Found(skill), Some(CandidateOpen::Skipped(cause))) => {
                Some(SkillDiagnostic {
                    path: skill.path.clone(),
                    source: skill.source,
                    scope: SkillDiagnosticScope::Candidate,
                    cause: *cause,
                })
            }
            _ => None,
        };
        let notice = self.discovery_notice(current_diagnostic);
        let budget = self.primary_budget(notice.as_ref());
        let model_output = match (resolution, validation, location) {
            (SkillResolution::Found(skill), Some(CandidateOpen::Current(candidate)), _) => {
                return Ok(Selected::Ready(Selection {
                    skill,
                    candidate,
                    notice,
                }));
            }
            (SkillResolution::AmbiguousName, _, _) => {
                format_ambiguous_skill(self.inventory.skills, name, budget)
            }
            (SkillResolution::NameLocationMismatch, _, Some(location))
            | (_, Some(CandidateOpen::NameMismatch), Some(location)) => {
                format_skill_location_mismatch(name, location, budget)
            }
            (_, _, Some(location)) => format_exact_skill_not_found(name, location, budget),
            (_, _, None) => format_missing_skill(name, budget),
        };
        Ok(Selected::Failed(
            self.finish(ExecuteResult::failure(model_output), notice),
        ))
    }

    fn discovery_notice(&self, additional: Option<SkillDiagnostic>) -> Option<String> {
        match additional {
            Some(diagnostic) => {
                let mut combined = self.inventory.diagnostics.to_vec();
                combined.push(diagnostic);
                diagnostic_summary(&combined)
            }
            None => diagnostic_summary(self.inventory.diagnostics),
        }
    }

    fn read(
        &self,
        selection: &Selection<'_>,
        resource: &str,
    ) -> Result<SkillResourceRead, SkillError> {
        #[cfg(test)]
        if let Some(after_selection) = self.after_selection {
            after_selection();
        }
        let candidate = &selection.candidate;
        let read = read_skill_resource(
            candidate,
            resource,
            self.limits.file,
            self.ceiling,
            self.cancellation,
        )?;
        check_cancelled(self.cancellation)?;
        if resource_is_skill_file(resource) {
            verify_read_identity(candidate, selection.skill, &read)?;
        } else {
            revalidate_primary_identity(candidate, selection.skill)?;
        }
        Ok(read)
    }

    fn load_chunk(
        &self,
        selection: Selection<'_>,
        resource: &str,
        offset: usize,
    ) -> Result<ExecuteResult, SkillError> {
        let read = self.read(&selection, resource)?;
        let result = self.chunk(selection.skill, resource, &read, offset);
        Ok(self.finish(result, selection.notice))
    }

    fn load_whole(
        &self,
        selection: Selection<'_>,
        resource: &str,
    ) -> Result<ExecuteResult, SkillError> {
        let read = self.read(&selection, resource)?;
        let budget = self.primary_budget(selection.notice.as_ref());
        let result = self.whole(selection.skill, resource, &read, budget);
        Ok(self.finish(result, selection.notice))
    }

    fn whole(
        &self,
        skill: &Skill,
        resource: &str,
        read: &SkillResourceRead,
        budget: usize,
    ) -> ExecuteResult {
        if read.observed_bytes > read.text.len() {
            return ExecuteResult::context_limit_failure(skill_file_blocked_marker(
                resource,
                read.observed_bytes,
                self.limits.file,
            ));
        }
        let chunk = self.limits.chunk;
        if chunk.source != ContextLimitSource::CompiledDefault
            && read.text.len() > chunk.effective_bytes()
        {
            return ExecuteResult::context_limit_failure(skill_chunk_blocked_marker(
                &skill.name,
                resource,
                read.text.len(),
                chunk,
                0,
            ));
        }
        let mut full = format!(
            "<skill_content name=\"{}\" location=\"",
            encoded_scalar(&skill.name)
        )
        .into_bytes();
        full.extend(encoded_bytes(skill.path.as_os_str().as_bytes()));
        full.extend_from_slice(
            format!(
                "\" resource=\"{}\" complete=\"true\">\n",
                encoded_scalar(resource)
            )
            .as_bytes(),
        );
        full.extend_from_slice(read.text.as_bytes());
        full.extend_from_slice(b"\n</skill_content>");
        let notice = (!is_model_safe_text(&full)).then(|| SANITIZED_NOTICE.to_owned());
        let model_output = sanitize_model_text_owned(full);
        if model_output.len() > budget {
            return ExecuteResult::failure(bounded_skill_error(
                format!(
                    "Complete skill content exceeds max_tool_result_bytes ({budget} bytes). No complete instructions were loaded."
                ),
                budget,
            ));
        }
        ExecuteResult::Loaded(ExecuteOutput {
            model_output,
            notice,
            diagnostic_notice: None,
            complete: true,
        })
    }

    fn chunk(
        &self,
        skill: &Skill,
        resource: &str,
        read: &SkillResourceRead,
        offset: usize,
    ) -> ExecuteResult {
        let text = read.text.as_str();
        if offset > text.len() || !text.is_char_boundary(offset) {
            return ExecuteResult::failure(OFFSET_BOUNDARY_FAILURE.to_owned());
        }
        let file_truncated = read.observed_bytes > text.len();
        if offset == text.len() && file_truncated {
            return ExecuteResult::context_limit_failure(skill_file_blocked_marker(
                resource,
                read.observed_bytes,
                self.limits.file,
            ));
        }
        let remaining = &text[offset..];
        let chunk_len =
            line_safe_prefix_length(remaining.as_bytes(), self.limits.chunk.effective_bytes());
        if !remaining.is_empty() && chunk_len == 0 {
            return ExecuteResult::context_limit_failure(skill_chunk_blocked_marker(
                &skill.name,
                resource,
                remaining.len(),
                self.limits.chunk,
                offset,
            ));
        }
        let next_offset = offset + chunk_len;
        let mut model_output = format!(
            "<skill_content name=\"{}\" resource=\"{}\" offset=\"{offset}\" next_offset=\"{next_offset}\">\n{}",
            encoded_scalar(&skill.name),
            encoded_scalar(resource),
            &remaining[..chunk_len]
        );
        let mut notice = None;
        if next_offset < text.len() {
            model_output.push('\n');
            model_output.push_str(&skill_chunk_truncated_marker(
                remaining.len(),
                self.limits.chunk,
                next_offset,
            ));
            notice = Some(skill_chunk_notice(
                &skill.name,
                resource,
                remaining.len(),
                self.limits.chunk,
                next_offset,
            ));
        } else if file_truncated {
            model_output.push('\n');
            model_output.push_str(&skill_file_blocked_marker(
                resource,
                read.observed_bytes,
                self.limits.file,
            ));
            notice = Some(skill_file_blocked_notice(
                &skill.name,
                resource,
                read.observed_bytes,
                self.limits.file,
            ));
        }
        model_output.push_str("\n</skill_content>");
        ExecuteResult::Loaded(ExecuteOutput {
            model_output,
            notice,
            ..ExecuteOutput::default()
        })
    }

    fn finish(&self, result: ExecuteResult, notice: Option<String>) -> ExecuteResult {
        let attach = |output| attach_discovery_notice(output, notice, self.max_tool_result_bytes);
        match result {
            ExecuteResult::Loaded(output) => ExecuteResult::Loaded(attach(output)),
            ExecuteResult::Failure(output) => ExecuteResult::Failure(attach(output)),
        }
    }
}

#[cfg(test)]
mod tests;
