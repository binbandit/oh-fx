use std::borrow::Cow;
use std::sync::Arc;

use ofx_contract::{
    BoxFuture, CallDescription, CallPresentation, Concurrency, PreparedCall, Tool, ToolActivity,
    ToolContext, ToolEffect, ToolOutput, ToolSpec, format_plain_action, parse_strict_json_value,
    parse_tool_args_object,
};
use ofx_text::write_scalar;
use serde_json::Value;

use crate::mcp_runtime::McpRuntime;
use crate::tool_mcp_registry::Projection;

pub(crate) const NAME: &str = "mcp_select_tool";
const DESCRIPTION: &str = "Exact-select one configured MCP/dynamic tool by name so its executable schema is advertised on the next model step. When to use: after discovering the exact specialized tool name in configured metadata. When NOT to use: guessing partial names, selecting built-in tools, or executing the dynamic tool directly.";
const SCHEMA: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"Exact dynamic MCP tool name discovered in configured metadata, such as mcp_server_tool."}},"required":["name"]}"#;
const INVALID: &str = "Invalid mcp_select_tool arguments.";
const NAME_REQUIRED: &str = "mcp_select_tool requires an exact dynamic tool name.";
const NO_RUNTIME: &str = "No MCP runtime is available.";
const PRESENTATION: CallPresentation = CallPresentation {
    activity: ToolActivity::Read,
    action_label: "Selecting MCP tool",
    completed_label: "Selected MCP tool",
    label_argument: "name",
    label_default: "dynamic tool",
};

pub struct McpSelectTool {
    spec: ToolSpec,
    runtime: Option<Arc<McpRuntime>>,
}

impl McpSelectTool {
    pub fn new(runtime: Option<Arc<McpRuntime>>) -> Self {
        Self {
            spec: ToolSpec {
                name: NAME.to_owned(),
                description: DESCRIPTION.to_owned(),
                input_schema: Cow::Borrowed(SCHEMA),
            },
            runtime,
        }
    }
}

impl Tool for McpSelectTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn provisional_presentation(&self) -> Option<CallPresentation> {
        Some(PRESENTATION)
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        let label = parse_tool_args_object(arguments).ok().map(|object| {
            PRESENTATION.label(
                object
                    .optional_string(PRESENTATION.label_argument)
                    .unwrap_or(PRESENTATION.label_default),
            )
        });
        Ok(Box::new(SelectCall {
            runtime: self.runtime.clone(),
            description: CallDescription {
                title: format_plain_action(NAME, label.as_ref()),
                label,
                activity: ToolActivity::Read,
                effect: ToolEffect::ReadOnly,
                concurrency: Concurrency::Serial,
            },
            name: decode(arguments).map_err(ToolOutput::failure),
        }))
    }
}

fn decode(arguments: &str) -> Result<String, &'static str> {
    let Ok(Value::Object(object)) = parse_strict_json_value(arguments.as_bytes()) else {
        return Err(INVALID);
    };
    object
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(NAME_REQUIRED)
}

struct SelectCall {
    runtime: Option<Arc<McpRuntime>>,
    description: CallDescription,
    name: Result<String, ToolOutput>,
}

impl PreparedCall for SelectCall {
    fn describe(&self) -> CallDescription {
        self.description.clone()
    }

    fn execute(self: Box<Self>, _context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let name = match self.name {
                Ok(name) => name,
                Err(failure) => return failure,
            };
            let Some(runtime) = self.runtime else {
                return ToolOutput::failure(NO_RUNTIME);
            };
            match runtime.tool_schema(&name).await {
                None => ToolOutput::failure(format!(
                    "Dynamic MCP tool not found or not allowed: {name}"
                )),
                Some(Projection::Rejected { output, notice }) => {
                    ToolOutput::failure(output).with_context_notices([notice])
                }
                Some(Projection::Selected { notice, .. }) => {
                    let mut encoded = String::new();
                    write_scalar(&mut encoded, &name);
                    ToolOutput::success(format!(
                        "Selected dynamic MCP tool `{encoded}`. Its executable schema will be available on the next model step; call `{encoded}` with arguments matching the selected schema."
                    ))
                    .with_context_notices(notice)
                    .selecting_tools([name])
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
