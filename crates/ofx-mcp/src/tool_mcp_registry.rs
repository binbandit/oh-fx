use std::fmt::Write as _;
use std::sync::Arc;

use ofx_config::{ContextLimit, line_safe_prefix_length};
use ofx_contract::{
    BoxFuture, CallDescription, Concurrency, DEFAULT_MAX_TOOL_RESULT_BYTES, PreparedCall, Tool,
    ToolActivity, ToolContext, ToolEffect, ToolOutput, ToolSpec, format_tool_execution_error_json,
    non_object_tool_arguments_json,
};
use ofx_text::write_scalar;
use serde_json::{Map, Value};

use crate::features::tools::Tool as CatalogTool;
use crate::server_lifecycle::{CallFailure, Server};
use crate::tool_names::ToolNames;
use crate::tool_operations::CallOptions;
use crate::tool_result::{model_output, restart_failed_output};

const SELECTED_SCHEMA_LIMIT: &str = "mcp_selected_schema_bytes";
const SERVER_INSTRUCTIONS_LIMIT: &str = "mcp_server_instructions_bytes";

#[derive(Debug, Clone, Copy)]
pub struct SchemaLimits {
    pub server_instructions: ContextLimit,
    pub selected_schema: ContextLimit,
}

pub(crate) fn publish_tools(
    servers: &[Arc<Server>],
    names: &mut ToolNames,
    reserved: &[String],
    limits: SchemaLimits,
) -> (Vec<Arc<dyn Tool>>, Vec<String>) {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut notices = Vec::new();
    for server in servers {
        let Some((catalog, instructions)) = server.catalog() else {
            continue;
        };
        for tool in &catalog.tools {
            let Ok(name) = names.name(reserved, &server.config.name, &tool.name) else {
                continue;
            };
            match project(&name, tool, instructions.as_deref(), limits) {
                Projection::Selected { spec, notice } => {
                    notices.extend(notice);
                    tools.push(Arc::new(McpTool {
                        spec: Arc::new(spec),
                        server: Arc::clone(server),
                        tool: tool.name.clone(),
                    }));
                }
                Projection::Rejected(notice) => notices.push(notice),
            }
        }
    }
    (tools, notices)
}

enum Projection {
    Selected {
        spec: ToolSpec,
        notice: Option<String>,
    },
    Rejected(String),
}

fn project(
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
    write_scalar(&mut description, &tool.description);
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
        return Projection::Rejected(schema_notice(
            name,
            "rejected",
            schema_bytes,
            schema_limit,
            SELECTED_SCHEMA_LIMIT,
        ));
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
    tool: String,
}

impl Tool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        let Ok(Value::Object(arguments)) = serde_json::from_str::<Value>(arguments) else {
            return Err(ToolOutput::failure(non_object_tool_arguments_json(
                &self.spec.name,
            )));
        };
        Ok(Box::new(McpCall {
            spec: Arc::clone(&self.spec),
            server: Arc::clone(&self.server),
            tool: self.tool.clone(),
            arguments,
        }))
    }
}

struct McpCall {
    spec: Arc<ToolSpec>,
    server: Arc<Server>,
    tool: String,
    arguments: Map<String, Value>,
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
        Some(function_schema(
            &self.spec.name,
            &self.spec.description,
            &self.spec.input_schema,
        ))
    }

    fn execute(self: Box<Self>, context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let arguments = Value::Object(self.arguments);
            let call = self.server.call(
                &self.tool,
                &arguments,
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
                    &self.tool,
                    &self.spec.name,
                    outcome,
                    DEFAULT_MAX_TOOL_RESULT_BYTES,
                ),
                Err(CallFailure::RestartFailed(failure)) => ToolOutput::failure(
                    restart_failed_output(server_name, &self.spec.name, &failure),
                ),
                Err(CallFailure::Mcp(error)) => ToolOutput::failure(
                    format_tool_execution_error_json(&self.spec.name, &error.to_string()),
                ),
            }
        })
    }
}
