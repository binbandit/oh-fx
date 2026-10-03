use std::fmt::Display;
use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};

use ofx_config::ContextLimits;
use ofx_contract::{
    ActionLabel, BoxFuture, CallDescription, CallPresentation, Concurrency,
    DEFAULT_MAX_TOOL_RESULT_BYTES, PreparedCall, Tool, ToolActivity, ToolArgValue, ToolArgs,
    ToolContext, ToolEffect, ToolOutput, ToolSpec, format_plain_action, parse_tool_args_object,
};
use ofx_skills::{
    CallPreparation, ExecuteOutput, ExecuteResult, Locations, PreparedSkill, RootPolicy,
    SkillDiscoveryContext, SkillInventory, SkillLoader, prepare_identity,
};
use tokio_util::sync::CancellationToken;

use crate::tool_args::{optional_integer, optional_string, parse_arguments};
use crate::tool_runtime::run_blocking;

const TOOL_NAME: &str = "skill";
const DESCRIPTION: &str = "Load an installed skill or one required relative text resource completely. Copy the exact advertised location. Resolve paths mentioned in skill instructions from the selected skill directory, not the workspace. Read referenced text with the same location and its relative resource path. When to use: the user explicitly invokes a listed skill or the task clearly matches one. When NOT to use: installing a missing skill.";
const INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"location":{"type":"string","description":"The exact advertised location of the selected skill."},"resource":{"type":"string","description":"Optional relative text resource within the selected skill. Omit or pass an empty string to read SKILL.md."}},"additionalProperties":false,"required":["location"]}"#;
const LOCATION_PREFIX: &str = "skill:";
const MAIN_RESOURCE: &str = "SKILL.md";

pub struct SkillTool {
    spec: ToolSpec,
    context: Arc<SkillContext>,
}

struct SkillContext {
    discovery: SkillDiscoveryContext,
    policy: RootPolicy,
    limits: ContextLimits,
    locations: RwLock<Arc<Locations>>,
}

impl SkillTool {
    pub fn new(
        discovery: SkillDiscoveryContext,
        policy: RootPolicy,
        limits: ContextLimits,
    ) -> Self {
        Self {
            spec: ToolSpec {
                name: TOOL_NAME.to_owned(),
                description: DESCRIPTION.to_owned(),
                input_schema: INPUT_SCHEMA,
            },
            context: Arc::new(SkillContext {
                discovery,
                policy,
                limits,
                locations: RwLock::default(),
            }),
        }
    }

    pub fn advertise(&self, locations: Locations) {
        *self
            .context
            .locations
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(locations);
    }
}

impl Tool for SkillTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn provisional_presentation(&self) -> Option<CallPresentation> {
        Some(CallPresentation {
            activity: ToolActivity::Read,
            action_label: "Loading skill",
            completed_label: "Loaded skill",
            label_argument: "location",
            label_default: "skill",
        })
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        let checked = SkillArgs::decode(arguments).and_then(|decoded| {
            let selected = self.context.select(&decoded)?;
            Ok((decoded, selected))
        });
        let resolved_name = checked
            .as_ref()
            .ok()
            .map(|(_, selected)| selected.skill.name.as_str());
        let label = label(arguments, resolved_name);
        let description = CallDescription {
            title: format_plain_action(TOOL_NAME, label.as_ref()),
            label,
            activity: ToolActivity::Read,
            effect: if checked.is_ok() {
                ToolEffect::ReadOnly
            } else {
                ToolEffect::None
            },
            concurrency: Concurrency::Parallel,
        };
        Ok(Box::new(SkillCall {
            description,
            checked,
            context: Arc::clone(&self.context),
        }))
    }
}

struct SkillCall {
    description: CallDescription,
    checked: Result<(SkillArgs, PreparedSkill), ToolOutput>,
    context: Arc<SkillContext>,
}

impl PreparedCall for SkillCall {
    fn describe(&self) -> CallDescription {
        self.description.clone()
    }

    fn refusal(&self) -> Option<&ToolOutput> {
        self.checked.as_ref().err()
    }

    fn execute(self: Box<Self>, tool_context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        let SkillCall {
            checked, context, ..
        } = *self;
        run_blocking(move || match checked {
            Ok((arguments, selected)) => {
                context.load(&arguments, &selected, &tool_context.cancellation)
            }
            Err(refusal) => refusal,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillArgs {
    name: Option<String>,
    location: Option<String>,
    resource: Option<String>,
    offset: usize,
}

impl SkillArgs {
    fn decode(args_json: &str) -> Result<Self, ToolOutput> {
        let arguments = parse_arguments(TOOL_NAME, args_json)?;
        let name = optional_string(TOOL_NAME, &arguments, "name")?;
        if name.is_none() && arguments.get("location").is_none() {
            return Err(ToolOutput::failure("skill requires an advertised location"));
        }
        let location = optional_string(TOOL_NAME, &arguments, "location")?;
        let resource = optional_string(TOOL_NAME, &arguments, "resource")?;
        if name.is_none() && arguments.get("offset").is_some() {
            return Err(ToolOutput::failure(
                "skill offset requires the legacy named resource form",
            ));
        }
        let offset = optional_integer(TOOL_NAME, &arguments, "offset", 0, "non-negative")?;
        Ok(Self {
            name,
            location,
            resource,
            offset: offset.unwrap_or(0),
        })
    }
}

impl SkillContext {
    fn select(&self, arguments: &SkillArgs) -> Result<PreparedSkill, ToolOutput> {
        match self.prepare(arguments) {
            CallPreparation::Selected(selected) => Ok(selected),
            CallPreparation::Failure(output) => Err(tool_output(ToolOutput::failure, output)),
        }
    }

    fn prepare(&self, arguments: &SkillArgs) -> CallPreparation {
        let name = arguments.name.as_deref();
        let locations = Arc::clone(
            &self
                .locations
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let location = match arguments
            .location
            .as_deref()
            .map(|location| locations.resolve(location))
            .transpose()
        {
            Ok(location) => location,
            Err(error) => return preparation_failure(error),
        };
        if let (Some(path), Some(requested)) = (&location, &arguments.location) {
            let advertised = locations
                .skills
                .iter()
                .any(|skill| skill.path.as_os_str() == path.as_os_str());
            if advertised || requested.starts_with(LOCATION_PREFIX) {
                return identity(
                    SkillInventory {
                        skills: &locations.skills,
                        diagnostics: &locations.diagnostics,
                    },
                    name,
                    Some(path),
                );
            }
        }
        let discovery = self.discovery.load_visible_skills(&self.policy);
        identity(
            SkillInventory {
                skills: &discovery.skills,
                diagnostics: &discovery.diagnostics,
            },
            name,
            location.as_deref(),
        )
    }

    fn load(
        &self,
        arguments: &SkillArgs,
        selected: &PreparedSkill,
        cancellation: &CancellationToken,
    ) -> ToolOutput {
        let skill = &selected.skill;
        let loader = SkillLoader::new(
            SkillInventory {
                skills: std::slice::from_ref(skill),
                diagnostics: &selected.diagnostics,
            },
            &self.discovery.symlink_authorities,
            &self.limits,
        )
        .with_max_tool_result_bytes(DEFAULT_MAX_TOOL_RESULT_BYTES)
        .with_cancellation(cancellation);
        let resource = arguments.resource.as_deref();
        let result = if arguments.name.is_some() {
            loader.load_by_identity(&skill.name, Some(&skill.path), resource, arguments.offset)
        } else {
            loader.load_whole_by_location(&skill.path, resource)
        };
        match result {
            Ok(ExecuteResult::Loaded(output)) => tool_output(ToolOutput::success, output),
            Ok(ExecuteResult::Failure(output)) => tool_output(ToolOutput::failure, output),
            Err(error) => ToolOutput::failure(format!("skill failed: {error}")),
        }
    }
}

fn tool_output(status: fn(String) -> ToolOutput, output: ExecuteOutput) -> ToolOutput {
    status(output.model_output).with_context_notices(
        [output.notice, output.diagnostic_notice]
            .into_iter()
            .flatten(),
    )
}

fn identity(
    inventory: SkillInventory<'_>,
    name: Option<&str>,
    location: Option<&Path>,
) -> CallPreparation {
    prepare_identity(&inventory, name, location, DEFAULT_MAX_TOOL_RESULT_BYTES)
        .unwrap_or_else(preparation_failure)
}

fn preparation_failure(error: impl Display) -> CallPreparation {
    CallPreparation::Failure(ExecuteOutput {
        model_output: format!(
            "skill failed: {error}. Refresh available skills and retry with an exact advertised location."
        ),
        ..ExecuteOutput::default()
    })
}

fn label(arguments: &str, resolved_name: Option<&str>) -> Option<ActionLabel> {
    let arguments = parse_tool_args_object(arguments).ok()?;
    if let Some(resource) = resource_label(&arguments) {
        return Some(ActionLabel {
            active: "Reading skill resource",
            completed: "Read skill resource",
            target: resource.to_owned(),
        });
    }
    let name = resolved_name
        .or_else(|| arguments.optional_string("name"))
        .unwrap_or(TOOL_NAME);
    Some(ActionLabel {
        active: "Loading skill",
        completed: "Loaded skill",
        target: name.to_owned(),
    })
}

fn resource_label(arguments: &ToolArgs) -> Option<&str> {
    let resource = match arguments.get("resource") {
        None => None,
        Some(ToolArgValue::String(resource)) => Some(resource.as_str()),
        Some(_) => return None,
    };
    let offset = match arguments.get("offset") {
        None => 0,
        Some(ToolArgValue::Integer(offset)) => usize::try_from(*offset).ok()?,
        Some(_) => return None,
    };
    let resource = resource
        .filter(|resource| !resource.is_empty())
        .unwrap_or(MAIN_RESOURCE);
    (offset != 0 || resource != MAIN_RESOURCE).then_some(resource)
}

#[cfg(test)]
mod tests;
