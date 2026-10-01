use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::Path;

use ofx_config::EMERGENCY_CEILING_BYTES;
use ofx_text::{encode_terminal_safe, sanitize_model_text_owned};

use super::failures::format_ambiguous_skill;
use super::{ExecuteResult, Selected, SkillError, SkillLoader};
use crate::skill_contract::{SKILL_FILE_NAME, Skill};
use crate::skill_runtime::{ExplicitSelection, collect_explicit_skill_selections};

const SECTION_HEADER: &str = "Explicitly invoked skill content for this query:\nUse every successfully loaded skill for this query. Report blocked or ambiguous requests.\nFollow each skill's complete instructions and required resources before substantive work.\nIf a skill cannot be followed, state the blocker instead of silently substituting another workflow.\n";
const AMBIGUOUS_FAILURE_BYTES: usize = 4096;
const INCOMPLETE_LOAD: &str = "Complete instructions were not loaded.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExplicitBinding<'a> {
    pub name: &'a str,
    pub path: &'a Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeTone {
    Neutral,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadNotice {
    pub tone: NoticeTone,
    pub body: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExplicitPromptSection {
    pub text: String,
    pub notice: Option<String>,
    pub diagnostic_notice: Option<String>,
    pub load_notice: Option<LoadNotice>,
    pub load_details: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlannedSkill<'a> {
    Bound(ExplicitBinding<'a>),
    Ambiguous(&'a str),
}

#[derive(Default)]
struct SectionBuilder {
    text: String,
    notices: String,
    diagnostic_notice: String,
    load_rows: String,
    load_details: String,
}

impl SkillLoader<'_> {
    pub fn build_explicit_prompt_section(
        &self,
        prompt: &str,
        bindings: &[ExplicitBinding<'_>],
    ) -> Result<ExplicitPromptSection, SkillError> {
        let plan = explicit_binding_plan(self.inventory.skills, prompt, bindings);
        if plan.is_empty() {
            return Ok(ExplicitPromptSection::default());
        }
        let loader = SkillLoader {
            max_tool_result_bytes: None,
            ..*self
        };
        let mut section = SectionBuilder {
            text: SECTION_HEADER.to_owned(),
            ..SectionBuilder::default()
        };
        let mut loaded_count = 0;
        for (index, entry) in plan.iter().enumerate() {
            section.load_rows.push_str(if index + 1 == plan.len() {
                "\n\u{2514} "
            } else {
                "\n\u{251c} "
            });
            match *entry {
                PlannedSkill::Bound(binding) => {
                    if section.append_skill(&loader, binding)? {
                        loaded_count += 1;
                    }
                }
                PlannedSkill::Ambiguous(name) => {
                    section.append_ambiguous(self.inventory.skills, name);
                }
            }
        }
        Ok(section.finish(loaded_count, plan.len() - loaded_count))
    }

    fn load_whole_by_identity(
        &self,
        binding: ExplicitBinding<'_>,
    ) -> Result<ExecuteResult, SkillError> {
        match self.select(binding.name, Some(binding.path))? {
            Selected::Ready(selection) => self.load_whole(selection, SKILL_FILE_NAME),
            Selected::Failed(failure) => Ok(failure),
        }
    }
}

fn explicit_binding_plan<'p>(
    skills: &'p [Skill],
    prompt: &str,
    bindings: &[ExplicitBinding<'p>],
) -> Vec<PlannedSkill<'p>> {
    let mut plan = Vec::new();
    let mut planned_paths: HashSet<&OsStr> = HashSet::new();
    for binding in bindings {
        if planned_paths.insert(binding.path.as_os_str()) {
            plan.push(PlannedSkill::Bound(*binding));
        }
    }
    for selection in collect_explicit_skill_selections(prompt, skills) {
        match selection {
            ExplicitSelection::Skill(index) => {
                let skill = &skills[index];
                if planned_paths.insert(skill.path.as_os_str()) {
                    plan.push(PlannedSkill::Bound(ExplicitBinding {
                        name: &skill.name,
                        path: &skill.path,
                    }));
                }
            }
            ExplicitSelection::Ambiguous(name) => {
                if !bindings
                    .iter()
                    .any(|binding| binding.name.eq_ignore_ascii_case(name))
                {
                    plan.push(PlannedSkill::Ambiguous(name));
                }
            }
        }
    }
    plan
}

impl SectionBuilder {
    fn append_skill(
        &mut self,
        loader: &SkillLoader<'_>,
        binding: ExplicitBinding<'_>,
    ) -> Result<bool, SkillError> {
        let remaining = loader.ceiling.saturating_sub(self.text.len());
        let result = loader.load_whole_by_identity(binding)?;
        let output = result.output();
        if output.model_output.len().saturating_add(1) > remaining {
            return Err(SkillError::SkillContextTooLarge);
        }
        self.text.push_str(&output.model_output);
        self.text.push('\n');
        if let Some(notice) = &output.notice {
            push_line(&mut self.notices, notice);
        }
        if let Some(notice) = &output.diagnostic_notice
            && self.diagnostic_notice.is_empty()
        {
            push_line(&mut self.diagnostic_notice, notice);
        }
        let failure = match &result {
            ExecuteResult::Loaded(output) if output.complete => None,
            ExecuteResult::Loaded(_) => Some(INCOMPLETE_LOAD),
            ExecuteResult::Failure(output) => Some(output.model_output.as_str()),
        };
        if let Some(detail) = failure {
            append_load_row(&mut self.load_details, binding.name, Some(detail));
            self.load_details.push('\n');
        }
        append_load_row(&mut self.load_rows, binding.name, failure.map(|_| ""));
        Ok(failure.is_none())
    }

    fn append_ambiguous(&mut self, skills: &[Skill], name: &str) {
        let failure = format_ambiguous_skill(skills, name, AMBIGUOUS_FAILURE_BYTES);
        self.text.push_str(&failure);
        self.text.push('\n');
        append_load_row(&mut self.load_rows, name, Some("ambiguous name"));
        append_load_row(&mut self.load_details, name, Some(&failure));
        self.load_details.push('\n');
    }

    fn finish(self, loaded: usize, failed: usize) -> ExplicitPromptSection {
        let (tone, body) = if failed == 0 {
            let plural = if loaded == 1 { "" } else { "s" };
            (
                NoticeTone::Neutral,
                format!("{loaded} requested skill{plural} loaded{}", self.load_rows),
            )
        } else {
            (
                NoticeTone::Warning,
                format!(
                    "Requested skills \u{b7} {loaded} loaded \u{b7} {failed} failed (ctrl+o for details){}",
                    self.load_rows
                ),
            )
        };
        ExplicitPromptSection {
            text: self.text,
            notice: non_empty(self.notices),
            diagnostic_notice: non_empty(self.diagnostic_notice),
            load_notice: Some(LoadNotice { tone, body }),
            load_details: non_empty(self.load_details),
        }
    }
}

fn push_line(output: &mut String, text: &str) {
    output.push_str(text);
    if !text.ends_with('\n') {
        output.push('\n');
    }
}

fn non_empty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

fn append_load_row(output: &mut String, name: &str, failure: Option<&str>) {
    let row = match failure {
        Some("") => format!("Could not load {name}"),
        Some(detail) => format!("Could not load {name}: {detail}"),
        None => format!("Loaded skill {name}"),
    };
    let sanitized = sanitize_model_text_owned(row.into_bytes());
    output.push_str(&encode_terminal_safe(sanitized.as_bytes(), EMERGENCY_CEILING_BYTES).text);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ofx_text::is_terminal_safe_char;

    use super::*;
    use crate::skill_contract::SkillSource;

    fn skill(name: &str, path: &str) -> Skill {
        Skill {
            name: name.to_owned(),
            description: String::new(),
            path: PathBuf::from(path),
            source: SkillSource::GlobalOhFx,
            read_authority: None,
        }
    }

    #[test]
    fn explicit_load_rows_keep_untrusted_names_terminal_safe_and_diagnostics_verbatim() {
        let mut row = String::new();
        append_load_row(
            &mut row,
            "workflow\n\x1b[2J",
            Some("Failed\r\nAPI_KEY=private-skill-secret"),
        );
        assert!(row.contains("Could not load workflow"));
        assert!(row.chars().all(is_terminal_safe_char));
        assert!(!row.contains('\n') && !row.contains('\x1b'));
        assert!(row.contains("private-skill-secret"));
    }

    #[test]
    fn explicit_binding_plan_preserves_supplied_order_and_adds_prompt_matches_once() {
        let skills = [
            skill("review", "/skills/review"),
            skill("release", "/skills/release"),
        ];
        let release = Path::new("/skills/release");
        let bindings = [
            ExplicitBinding {
                name: "release",
                path: release,
            },
            ExplicitBinding {
                name: "release duplicate",
                path: release,
            },
        ];
        assert_eq!(
            explicit_binding_plan(&skills, "$review these changes", &bindings),
            [
                PlannedSkill::Bound(bindings[0]),
                PlannedSkill::Bound(ExplicitBinding {
                    name: "review",
                    path: Path::new("/skills/review"),
                }),
            ]
        );
    }

    #[test]
    fn explicit_binding_plan_lets_a_binding_settle_an_ambiguous_prompt_name() {
        let skills = [
            skill("review", "/workspace/review"),
            skill("review", "/global/review"),
        ];
        assert_eq!(
            explicit_binding_plan(&skills, "$review", &[]),
            [PlannedSkill::Ambiguous("review")]
        );
        let bound = [ExplicitBinding {
            name: "REVIEW",
            path: Path::new("/global/review"),
        }];
        assert_eq!(
            explicit_binding_plan(&skills, "$review", &bound),
            [PlannedSkill::Bound(bound[0])]
        );
    }
}
