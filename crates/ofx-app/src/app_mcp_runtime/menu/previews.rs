use std::sync::Arc;

use ofx_contract::{McpMenuArgument, McpMenuPreview, McpMenuPreviewTarget, UiEvent};
use ofx_mcp::{FeatureFailure, McpRuntime, MenuToolSchema, ResourceData};
use ofx_text::encode_terminal_safe_inline;
use serde_json::Value;

use super::super::McpHost;

const MAX_PREVIEW_BYTES: usize = 64 * 1024;
const TOOL_HEADING: &str = "MCP tool schema · untrusted metadata\n\n";
const RESOURCE_HEADING: &str = "MCP resource · untrusted content\n\n";
const PROMPT_HEADING: &str = "MCP prompt · untrusted content\n\n";
const BINARY_CONTENT: &str = "[binary MCP resource content omitted]";
const TOOL_NOT_FOUND: &str = "McpToolNotFound";
const ACCESS_DENIED: &str = "McpAccessDenied";
const PROTOCOL_ERROR: &str = "McpProtocolError";

impl McpHost {
    pub(super) fn load_menu_preview(&self, generation: u64, target: McpMenuPreviewTarget) {
        let runtime = Arc::clone(&self.runtime);
        self.start_menu_operation(generation, "preview", async move {
            preview(&runtime, target)
                .await
                .map(|preview| UiEvent::McpMenuPreviewed {
                    generation,
                    preview,
                })
        });
    }
}

async fn preview(
    runtime: &McpRuntime,
    target: McpMenuPreviewTarget,
) -> Result<McpMenuPreview, String> {
    match target {
        McpMenuPreviewTarget::Tool(name) => {
            match runtime
                .menu_tool_schema(&name)
                .await
                .map_err(|error| error.to_string())?
            {
                MenuToolSchema::Selected(schema) => Ok(menu_preview(TOOL_HEADING, &schema)),
                MenuToolSchema::Rejected => Err(ACCESS_DENIED.to_owned()),
                MenuToolSchema::Missing => Err(TOOL_NOT_FOUND.to_owned()),
            }
        }
        McpMenuPreviewTarget::Resource { server, uri } => {
            let contents = runtime
                .read_resource(&server, &uri)
                .await
                .map_err(failure_name)?;
            let parts: Vec<&str> = contents
                .iter()
                .map(|content| match &content.data {
                    ResourceData::Text(text) => text.as_str(),
                    ResourceData::Blob(_) => BINARY_CONTENT,
                })
                .collect();
            Ok(menu_preview(RESOURCE_HEADING, &parts.join("\n\n")))
        }
        McpMenuPreviewTarget::Prompt {
            server,
            name,
            arguments,
        } => {
            let result = runtime
                .get_prompt(&server, &name, &arguments_json(&arguments))
                .await
                .map_err(failure_name)?;
            let messages: Vec<String> = result
                .messages
                .iter()
                .map(|message| format!("{}: {}", message.role.as_str(), message.content_json))
                .collect();
            Ok(menu_preview(PROMPT_HEADING, &messages.join("\n\n")))
        }
    }
}

fn arguments_json(arguments: &[McpMenuArgument]) -> String {
    let fields: Vec<String> = arguments
        .iter()
        .filter(|argument| !argument.value.is_empty())
        .map(|argument| {
            format!(
                "{}:{}",
                Value::from(argument.name.as_str()),
                Value::from(argument.value.as_str())
            )
        })
        .collect();
    format!("{{{}}}", fields.join(","))
}

fn failure_name(failure: FeatureFailure) -> String {
    match failure {
        FeatureFailure::Error(error) => error.to_string(),
        FeatureFailure::Diagnostic(_) => PROTOCOL_ERROR.to_owned(),
    }
}

fn menu_preview(heading: &str, raw: &str) -> McpMenuPreview {
    let insert = raw[..raw.floor_char_boundary(MAX_PREVIEW_BYTES)].to_owned();
    let display = encode_terminal_safe_inline(insert.as_bytes(), MAX_PREVIEW_BYTES).text;
    McpMenuPreview {
        display: format!("{heading}{display}"),
        insert,
    }
}

#[cfg(test)]
mod tests;
