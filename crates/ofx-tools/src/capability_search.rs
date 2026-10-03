use std::sync::Arc;

use ofx_config::{ContextLimitName, ContextLimits};
use ofx_contract::{
    ActionLabel, BoxFuture, CallDescription, Concurrency, PreparedCall, Tool, ToolActivity,
    ToolContext, ToolEffect, ToolOutput, ToolSpec, format_plain_action,
};
use ofx_skills::{RootPolicy, SkillDiscoveryContext, diagnostic_summary, search_skills};
use ofx_text::PreparedQuery;

use crate::tool_args::{optional_string, parse_arguments};
use crate::tool_runtime::run_blocking;

const NAME: &str = "capability_search";
const DESCRIPTION: &str = "Find installed skills and configured MCP tools for a described capability. Optionally restrict MCP results to one exact configured server. Results describe this query; no_match does not rule out another query. Use returned skill locations with skill. Matching MCP schemas are loaded automatically within the schema budget; call advertised tools directly or use mcp_select_tool for explicit selection. Do not guess identities.";
const SCHEMA: &str = r#"{"type":"object","properties":{"query":{"type":"string","minLength":1,"maxLength":4096,"description":"Natural-language capability needed for the current task."},"server":{"type":"string","minLength":1,"description":"Optional exact configured MCP server alias."}},"additionalProperties":false,"required":["query"]}"#;

pub struct CapabilitySearch {
    spec: ToolSpec,
    context: Arc<SearchContext>,
}

struct SearchContext {
    discovery: SkillDiscoveryContext,
    policy: RootPolicy,
    limits: ContextLimits,
    max_tool_result_bytes: usize,
}

impl CapabilitySearch {
    pub fn new(
        discovery: SkillDiscoveryContext,
        policy: RootPolicy,
        limits: ContextLimits,
        max_tool_result_bytes: usize,
    ) -> Self {
        Self {
            spec: ToolSpec {
                name: NAME.to_owned(),
                description: DESCRIPTION.to_owned(),
                input_schema: SCHEMA,
            },
            context: Arc::new(SearchContext {
                discovery,
                policy,
                limits,
                max_tool_result_bytes,
            }),
        }
    }
}

struct Input {
    query: PreparedQuery,
    server: Option<String>,
}

impl Input {
    fn decode(arguments: &str) -> Result<Self, ToolOutput> {
        let object = parse_arguments(NAME, arguments)?;
        let query = optional_string(NAME, &object, "query")?
            .ok_or_else(|| ToolOutput::failure("capability_search field \"query\" is required"))?;
        if query.is_empty() {
            return Err(ToolOutput::failure(
                "capability_search field \"query\" must not be empty",
            ));
        }
        let query = PreparedQuery::prepare(query).map_err(|_| {
            ToolOutput::failure("capability_search query must not exceed 4096 bytes")
        })?;
        let server = optional_string(NAME, &object, "server")?;
        if server.as_deref() == Some("") {
            return Err(ToolOutput::failure(
                "capability_search field \"server\" must not be empty",
            ));
        }
        Ok(Self { query, server })
    }

    fn label(&self) -> ActionLabel {
        ActionLabel {
            active: "Searching capabilities",
            completed: "Searched capabilities",
            target: self
                .server
                .as_deref()
                .unwrap_or_else(|| self.query.raw())
                .to_owned(),
        }
    }
}

impl Tool for CapabilitySearch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        let checked = Input::decode(arguments);
        let label = checked.as_ref().ok().map(Input::label);
        let description = CallDescription {
            title: format_plain_action(NAME, label.as_ref()),
            label,
            activity: ToolActivity::Read,
            effect: if checked.is_ok() {
                ToolEffect::ReadOnly
            } else {
                ToolEffect::None
            },
            concurrency: Concurrency::Parallel,
        };
        Ok(Box::new(SearchCall {
            context: Arc::clone(&self.context),
            description,
            checked,
        }))
    }
}

struct SearchCall {
    context: Arc<SearchContext>,
    description: CallDescription,
    checked: Result<Input, ToolOutput>,
}

impl PreparedCall for SearchCall {
    fn describe(&self) -> CallDescription {
        self.description.clone()
    }
    fn refusal(&self) -> Option<&ToolOutput> {
        self.checked.as_ref().err()
    }
    fn execute(self: Box<Self>, _tool_context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        let SearchCall {
            context, checked, ..
        } = *self;
        run_blocking(move || match checked {
            Ok(input) => context.search(&input),
            Err(output) => output,
        })
    }
}

impl SearchContext {
    fn search(&self, input: &Input) -> ToolOutput {
        let output_cap = self.max_tool_result_bytes.min(16 * 1024);
        let mut notices = Vec::new();
        let result = if input.server.is_none() {
            let domain_cap = if output_cap > 512 {
                (output_cap - 512) / 2
            } else {
                output_cap
            };
            let discovery = self.discovery.load_visible_skills(&self.policy);
            notices.extend(diagnostic_summary(&discovery.diagnostics));
            match search_skills(
                &input.query,
                &discovery.skills,
                self.limits
                    .get(ContextLimitName::SkillDescriptionBytes)
                    .effective_bytes(),
                domain_cap,
            ) {
                Ok(result) => combine(
                    &result.items_json,
                    result.count,
                    result.total_matches,
                    output_cap,
                ),
                Err(error) => {
                    ToolOutput::failure(format!("capability_search skill search failed: {error}"))
                }
            }
        } else {
            combine("", 0, 0, output_cap)
        };
        result.with_context_notices(notices)
    }
}

fn combine(items: &str, count: usize, total: usize, max_bytes: usize) -> ToolOutput {
    let output = format!(
        "{{\"skills\":[{items}],\"mcp_tools\":[],\"counts\":{{\"skills\":{count},\"mcp_tools\":0}},\"total_matches\":{{\"skills\":{total},\"mcp_tools\":0}},\"mcp_state\":\"unavailable\"}}"
    );
    if output.len() > max_bytes {
        ToolOutput::failure(
            "capability_search combined search failed: CapabilitySearchResultLimitTooSmall",
        )
    } else {
        ToolOutput::success(output)
    }
}

#[cfg(test)]
mod tests;
