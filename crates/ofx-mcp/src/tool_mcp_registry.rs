use std::fmt::Write as _;
use std::sync::Arc;

use ofx_config::{ContextLimit, ContextLimitName, ContextLimits, line_safe_prefix_length};
use ofx_contract::{
    BoxFuture, CallDescription, Concurrency, DEFAULT_MAX_TOOL_RESULT_BYTES, PreparedCall, Tool,
    ToolActivity, ToolContext, ToolEffect, ToolOutput, ToolSpec, format_tool_execution_error_json,
};
use ofx_text::write_scalar;
use serde_json::Value;

use crate::features::tools::{Limits, Tool as CatalogTool, ToolCatalog, validate_arguments};
use crate::server_lifecycle::{Advertised, CallFailure, Server};
use crate::tool_names::ToolNames;
use crate::tool_operations::CallOptions;
use crate::tool_result::{model_output, restart_failed_output};

const SELECTED_SCHEMA_LIMIT: &str = "mcp_selected_schema_bytes";
const SERVER_INSTRUCTIONS_LIMIT: &str = "mcp_server_instructions_bytes";
const DEFINITION_CHANGED: &str = "MCP tool definition changed before execution. Its current schema is loaded; review it before issuing a new call.";
const DEFINITION_WITHDRAWN: &str = "MCP tool definition changed before execution and is no longer available. Search for current tools.";

#[derive(Debug, Clone, Copy)]
pub(crate) struct SchemaLimits {
    pub(crate) server_instructions: ContextLimit,
    pub(crate) selected_schema: ContextLimit,
}

impl From<&ContextLimits> for SchemaLimits {
    fn from(limits: &ContextLimits) -> Self {
        Self {
            server_instructions: limits.get(ContextLimitName::McpServerInstructionsBytes),
            selected_schema: limits.get(ContextLimitName::McpSelectedSchemaBytes),
        }
    }
}

pub(crate) fn publish_tools(
    servers: &[Arc<Server>],
    names: &mut ToolNames,
    reserved: &[String],
    limits: SchemaLimits,
) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for server in servers {
        let Some((catalog, instructions)) = server.catalog() else {
            continue;
        };
        tools.extend(
            publish_server(
                server,
                &catalog,
                instructions.as_ref(),
                names,
                reserved,
                limits,
            )
            .into_iter()
            .map(|tool| Arc::new(tool) as Arc<dyn Tool>),
        );
    }
    tools
}

fn publish_server(
    server: &Arc<Server>,
    catalog: &ToolCatalog,
    instructions: Option<&Arc<str>>,
    names: &mut ToolNames,
    reserved: &[String],
    limits: SchemaLimits,
) -> Vec<McpTool> {
    let mut tools = Vec::new();
    for tool in &catalog.tools {
        let Ok(name) = names.name(reserved, &server.config.name, &tool.name) else {
            continue;
        };
        if let Projection::Selected { spec, .. } =
            project(&name, tool, instructions.map(AsRef::as_ref), limits)
        {
            tools.push(McpTool {
                spec: Arc::new(spec),
                server: Arc::clone(server),
                advertised: Arc::new(Advertised {
                    tool: tool.clone(),
                    instructions: instructions.cloned(),
                }),
                limits,
            });
        }
    }
    tools
}

pub(crate) fn schema_for(
    servers: &[Arc<Server>],
    names: &mut ToolNames,
    reserved: &[String],
    name: &str,
    limits: SchemaLimits,
) -> Option<Projection> {
    servers.iter().find_map(|server| {
        let (catalog, instructions) = server.catalog()?;
        catalog.tools.iter().find_map(|tool| {
            let prefixed = names.name(reserved, &server.config.name, &tool.name).ok()?;
            (prefixed == name).then(|| project(name, tool, instructions.as_deref(), limits))
        })
    })
}

pub(crate) enum Projection {
    Selected {
        spec: ToolSpec,
        notice: Option<String>,
    },
    Rejected {
        output: String,
        notice: String,
    },
}

pub(crate) fn project(
    name: &str,
    tool: &CatalogTool,
    instructions: Option<&str>,
    limits: SchemaLimits,
) -> Projection {
    let instruction_limit = limits.server_instructions;
    let observed = instructions.map_or(0, str::len);
    let kept = instructions.map(|text| {
        text.get(..line_safe_prefix_length(text.as_bytes(), instruction_limit.effective_bytes()))
            .unwrap_or_default()
    });
    let truncated = kept.is_some_and(|text| text.len() < observed);
    let mut description = String::new();
    write_scalar(&mut description, tool.catalog_description());
    if let Some(kept) = kept {
        description.push_str("\n\nServer instructions: ");
        write_scalar(&mut description, kept);
        if truncated {
            let _ = write!(
                description,
                "\n<context_limit name=\"{SERVER_INSTRUCTIONS_LIMIT}\" action=\"truncated\" observed_bytes=\"{observed}\" effective_bytes=\"{}\" source=\"{}\" override=\"--context-limit {SERVER_INSTRUCTIONS_LIMIT}=BYTES|off\" />",
                instruction_limit.effective_bytes(),
                instruction_limit.source.label(),
            );
        }
    }
    let input_schema = tool.input_schema.to_string();
    let schema_bytes = function_schema(name, &description, &input_schema).len();
    let schema_limit = limits.selected_schema;
    if schema_bytes > schema_limit.effective_bytes() {
        let mut encoded = String::new();
        write_scalar(&mut encoded, name);
        return Projection::Rejected {
            output: format!(
                r#"{{"context_limit_rejection":{{"name":"{SELECTED_SCHEMA_LIMIT}","tool":{},"action":"rejected","observed_bytes":{schema_bytes},"effective_bytes":{},"source":"{}","override":"--context-limit {SELECTED_SCHEMA_LIMIT}=BYTES|off"}}}}"#,
                Value::from(encoded),
                schema_limit.effective_bytes(),
                schema_limit.source.label(),
            ),
            notice: schema_notice(
                name,
                "rejected",
                schema_bytes,
                schema_limit,
                SELECTED_SCHEMA_LIMIT,
            ),
        };
    }
    Projection::Selected {
        spec: ToolSpec {
            name: name.to_owned(),
            description,
            input_schema: input_schema.into(),
        },
        notice: truncated.then(|| {
            schema_notice(
                name,
                "instructions truncated",
                observed,
                instruction_limit,
                SERVER_INSTRUCTIONS_LIMIT,
            )
        }),
    }
}

pub(crate) fn selected_schema(spec: &ToolSpec) -> String {
    function_schema(&spec.name, &spec.description, &spec.input_schema)
}

fn function_schema(name: &str, description: &str, input_schema: &str) -> String {
    format!(
        "{{\"type\":\"function\",\"name\":{},\"description\":{},\"inputSchema\":{input_schema}}}",
        Value::from(name),
        Value::from(description)
    )
}

fn schema_notice(
    name: &str,
    action: &str,
    observed: usize,
    limit: ContextLimit,
    limit_name: &str,
) -> String {
    let mut encoded = String::new();
    write_scalar(&mut encoded, name);
    format!(
        "[context] MCP schema \"{encoded}\" {action}: observed={observed} bytes effective={} bytes source={}; override with --context-limit {limit_name}=BYTES|off",
        limit.effective_bytes(),
        limit.source.label()
    )
}

struct McpTool {
    spec: Arc<ToolSpec>,
    server: Arc<Server>,
    advertised: Arc<Advertised>,
    limits: SchemaLimits,
}

impl Tool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        if let Err(error) = validate_arguments(arguments, Limits::default()) {
            return Err(ToolOutput::failure(format!(
                "Invalid arguments for MCP tool {}: {error}",
                self.spec.name
            )));
        }
        Ok(Box::new(McpCall {
            spec: Arc::clone(&self.spec),
            server: Arc::clone(&self.server),
            advertised: Arc::clone(&self.advertised),
            limits: self.limits,
            arguments: arguments.to_owned(),
        }))
    }
}

struct McpCall {
    spec: Arc<ToolSpec>,
    server: Arc<Server>,
    advertised: Arc<Advertised>,
    limits: SchemaLimits,
    arguments: String,
}

impl McpCall {
    fn changed_definition(&self) -> ToolOutput {
        let name = &self.spec.name;
        let current = self.server.catalog().and_then(|(catalog, instructions)| {
            catalog
                .get(&self.advertised.tool.name)
                .map(|tool| project(name, tool, instructions.as_deref(), self.limits))
        });
        match current {
            Some(Projection::Selected { notice, .. }) => ToolOutput::failure(DEFINITION_CHANGED)
                .with_context_notices(notice)
                .selecting_tools([name.clone()]),
            Some(Projection::Rejected { output, notice }) => ToolOutput::failure(output)
                .with_context_notices([notice])
                .retiring_tool(name.clone()),
            None => ToolOutput::failure(DEFINITION_WITHDRAWN).retiring_tool(name.clone()),
        }
    }
}

impl PreparedCall for McpCall {
    fn describe(&self) -> CallDescription {
        CallDescription {
            title: format!("MCP: {}", self.spec.name),
            label: None,
            activity: ToolActivity::Command,
            effect: ToolEffect::Mutating,
            concurrency: Concurrency::Serial,
        }
    }

    fn mcp_tool(&self) -> bool {
        true
    }

    fn review_schema(&self) -> Option<String> {
        Some(selected_schema(&self.spec))
    }

    fn execute(self: Box<Self>, context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let call = self.server.call(
                &self.advertised,
                &self.arguments,
                CallOptions {
                    max_tool_result_bytes: DEFAULT_MAX_TOOL_RESULT_BYTES,
                    progress: None,
                },
            );
            let Some(outcome) = context.cancellation.run_until_cancelled(call).await else {
                return ToolOutput::failure(format_tool_execution_error_json(
                    &self.spec.name,
                    "Cancelled",
                ));
            };
            let server_name = &self.server.config.name;
            match outcome {
                Ok(outcome) => model_output(
                    server_name,
                    &self.advertised.tool.name,
                    &self.spec.name,
                    outcome,
                    DEFAULT_MAX_TOOL_RESULT_BYTES,
                ),
                Err(CallFailure::RestartFailed(failure)) => ToolOutput::failure(
                    restart_failed_output(server_name, &self.spec.name, &failure),
                ),
                Err(CallFailure::DefinitionChanged) => self.changed_definition(),
                Err(CallFailure::Mcp(error)) => ToolOutput::failure(
                    format_tool_execution_error_json(&self.spec.name, &error.to_string()),
                ),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use serde_json::json;

    use super::*;
    use crate::features::tools::ToolCatalog;
    use crate::mcp_contract::McpServerConfig;
    use crate::server_transport::ConnectOptions;

    fn tool(name: &str) -> CatalogTool {
        CatalogTool {
            name: name.to_owned(),
            title: None,
            description: format!("{name} tool"),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            icons: None,
            annotations: None,
            meta: None,
        }
    }

    #[test]
    fn every_tool_of_a_server_shares_one_copy_of_its_instructions() {
        let limits = SchemaLimits::from(&ContextLimits::default());
        let server = Arc::new(Server::new(
            McpServerConfig::stdio("fixture", "/bin/true", Vec::new()),
            ConnectOptions::default(),
            Arc::new(AtomicU64::new(0)),
        ));
        let catalog = ToolCatalog {
            tools: (0..64).map(|index| tool(&format!("t{index}"))).collect(),
        };
        let instructions: Arc<str> = Arc::from("x".repeat(512 * 1024));
        let tools = publish_server(
            &server,
            &catalog,
            Some(&instructions),
            &mut ToolNames::default(),
            &[],
            limits,
        );
        assert_eq!(tools.len(), 64);
        for published in &tools {
            let shared = published.advertised.instructions.as_ref().unwrap();
            assert!(Arc::ptr_eq(shared, &instructions));
        }
        assert_eq!(Arc::strong_count(&instructions), 65);
    }
}
