use std::fmt::Write as _;
use std::sync::Arc;

use ofx_config::{ContextLimitName, ContextLimits};
use ofx_contract::{
    ActionLabel, BoxFuture, CallDescription, Concurrency, McpSearchRequest, McpSearchResult,
    McpToolSearch, PreparedCall, Tool, ToolActivity, ToolContext, ToolEffect, ToolOutput, ToolSpec,
    format_plain_action,
};
use ofx_skills::{
    RootPolicy, SkillDiscoveryContext, SkillSearchResult, diagnostic_summary, search_skills,
};
use ofx_text::PreparedQuery;
use serde_json::Value;

use crate::tool_args::{optional_string, parse_arguments};
use crate::tool_runtime::run_blocking;

const NAME: &str = "capability_search";
const DESCRIPTION: &str = "Find installed skills and configured MCP tools for a described capability. Optionally restrict MCP results to one exact configured server. Results describe this query; no_match does not rule out another query. Use returned skill locations with skill. Matching MCP schemas are loaded automatically within the schema budget; call advertised tools directly or use mcp_select_tool for explicit selection. Do not guess identities.";
const SCHEMA: &str = r#"{"type":"object","properties":{"query":{"type":"string","minLength":1,"maxLength":4096,"description":"Natural-language capability needed for the current task."},"server":{"type":"string","minLength":1,"description":"Optional exact configured MCP server alias."}},"additionalProperties":false,"required":["query"]}"#;
const LARGE_RESULT_THRESHOLD_BYTES: usize = 16 * 1024;
const NO_RUNTIME: &str = r#"{"tools":[],"count":0}"#;
const UNAVAILABLE: &str = r#"{"tools":[],"count":0,"state":"unavailable"}"#;

pub struct CapabilitySearch {
    spec: ToolSpec,
    context: Arc<SearchContext>,
}

#[derive(Clone)]
struct SearchContext {
    discovery: SkillDiscoveryContext,
    policy: RootPolicy,
    limits: ContextLimits,
    max_tool_result_bytes: usize,
    interactive_host: bool,
    mcp: Option<Arc<dyn McpToolSearch>>,
}

impl CapabilitySearch {
    #[must_use]
    pub fn with_interactive_host(mut self, interactive: bool) -> Self {
        Arc::make_mut(&mut self.context).interactive_host = interactive;
        self
    }

    #[must_use]
    pub fn searching_mcp(&self, mcp: Arc<dyn McpToolSearch>) -> Self {
        Self {
            spec: self.spec.clone(),
            context: Arc::new(SearchContext {
                mcp: Some(mcp),
                ..(*self.context).clone()
            }),
        }
    }

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
                input_schema: SCHEMA.into(),
            },
            context: Arc::new(SearchContext {
                discovery,
                policy,
                limits,
                max_tool_result_bytes,
                interactive_host: false,
                mcp: None,
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
}

impl Tool for CapabilitySearch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        let checked = Input::decode(arguments);
        let label = parse_arguments(NAME, arguments)
            .ok()
            .map(|object| ActionLabel {
                active: "Searching capabilities",
                completed: "Searched capabilities",
                target: object
                    .optional_string("server")
                    .filter(|value| !value.is_empty())
                    .or_else(|| object.optional_string("query"))
                    .unwrap_or("capabilities")
                    .to_owned(),
            });
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
        Box::pin(async move {
            match checked {
                Ok(input) => context.search(Arc::new(input)).await,
                Err(output) => output,
            }
        })
    }
}

impl SearchContext {
    async fn search(self: Arc<Self>, input: Arc<Input>) -> ToolOutput {
        let output_cap = self.max_tool_result_bytes.min(LARGE_RESULT_THRESHOLD_BYTES);
        let searches_skills = input.server.is_none();
        let domain_cap = if searches_skills && output_cap > 512 {
            (output_cap - 512) / 2
        } else {
            output_cap
        };
        let mut notices = Vec::new();
        let skills = if searches_skills {
            let context = Arc::clone(&self);
            let query = Arc::clone(&input);
            let (result, diagnostics) =
                run_blocking(move || context.search_skills(&query.query, domain_cap)).await;
            notices.extend(diagnostics);
            match result {
                Ok(result) => Some(result),
                Err(error) => {
                    return ToolOutput::failure(format!(
                        "capability_search skill search failed: {error}"
                    ))
                    .with_context_notices(notices);
                }
            }
        } else {
            None
        };
        let mcp = self.search_mcp(&input, domain_cap).await;
        notices.extend(mcp.notice);
        match combine(skills.as_ref(), &mcp.model_output, output_cap) {
            Ok(output) => ToolOutput::success(output),
            Err(error) => {
                ToolOutput::failure(format!("capability_search combined search failed: {error}"))
            }
        }
        .with_context_notices(notices)
    }

    fn search_skills(
        &self,
        query: &PreparedQuery,
        max_bytes: usize,
    ) -> (
        Result<SkillSearchResult, ofx_skills::SkillSearchError>,
        Vec<String>,
    ) {
        let discovery = self.discovery.load_visible_skills(&self.policy);
        let result = search_skills(
            query,
            &discovery.skills,
            self.limits
                .get(ContextLimitName::SkillDescriptionBytes)
                .effective_bytes(),
            max_bytes,
        );
        (
            result,
            diagnostic_summary(&discovery.diagnostics)
                .into_iter()
                .collect(),
        )
    }

    async fn search_mcp(&self, input: &Input, max_bytes: usize) -> McpSearchResult {
        match &self.mcp {
            Some(mcp) => {
                mcp.search_tools(McpSearchRequest {
                    query: &input.query,
                    server: input.server.as_deref(),
                    result_bytes: (!self.interactive_host).then_some(max_bytes),
                })
                .await
            }
            None if self.interactive_host => McpSearchResult::plain(NO_RUNTIME),
            None => McpSearchResult::plain(UNAVAILABLE),
        }
    }
}

fn combine(
    skills: Option<&SkillSearchResult>,
    mcp_output: &str,
    max_bytes: usize,
) -> Result<String, &'static str> {
    let mcp: Value =
        serde_json::from_str(mcp_output).map_err(|_| "InvalidCapabilitySearchResult")?;
    let tools = mcp
        .get("tools")
        .and_then(Value::as_array)
        .ok_or("InvalidCapabilitySearchResult")?;
    let skill_count = skills.map_or(0, |result| result.count);
    let skill_total = skills.map_or(0, |result| result.total_matches);
    let mcp_total = mcp
        .get("total_matches")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let field = |name: &str| mcp.get(name);
    let mut output = format!(
        r#"{{"skills":[{}],"mcp_tools":[{}],"counts":{{"skills":{skill_count},"mcp_tools":{}}},"total_matches":{{"skills":{skill_total},"mcp_tools":{mcp_total}}}"#,
        skills.map_or("", |result| result.items_json.as_str()),
        tools
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(","),
        tools.len(),
    );
    if skill_total == 0
        && mcp_total == 0
        && ["authentication_required", "state", "error"]
            .iter()
            .all(|name| field(name).is_none())
    {
        output.push_str(r#","state":"no_match""#);
    }
    for (name, key) in [
        ("authentication_required", "authentication_required"),
        ("state", "mcp_state"),
        ("error", "mcp_error"),
        ("context_limit", "mcp_context_limit"),
    ] {
        if let Some(value) = field(name) {
            let _ = write!(output, r#","{key}":{value}"#);
        }
    }
    output.push('}');
    if output.len() > max_bytes {
        return Err("CapabilitySearchResultLimitTooSmall");
    }
    Ok(output)
}

#[cfg(test)]
mod tests;
