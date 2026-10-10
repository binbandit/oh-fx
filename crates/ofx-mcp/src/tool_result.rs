use ofx_contract::{ToolOutput, prepare_model_output};
use ofx_jsonrpc::RpcError;
use ofx_text::sanitize_model_text_owned;
use serde_json::{Map, Value};

use crate::features::tools::ToolCallOutcome;

const UNSUPPORTED_MEDIA: &str = "unsupported media; content was not sent to the model";
const UNSENT_BINARY_RESOURCE: &str = "binary resource content was not sent to the model";
const TEXT_FIELDS: [&str; 4] = ["text", "data", "blob", "content"];

pub(crate) fn model_output(
    server: &str,
    tool: &str,
    prefixed_name: &str,
    outcome: ToolCallOutcome,
    max_bytes: usize,
) -> ToolOutput {
    match outcome {
        ToolCallOutcome::Complete(complete) => {
            let mut result = complete.result;
            if let Some(content) = result.get_mut("content") {
                project_media_for_text(content);
            }
            let text = serialize_capped(server, tool, &result, max_bytes);
            if complete.is_error {
                ToolOutput::failure(text)
            } else {
                ToolOutput::success(text)
            }
        }
        ToolCallOutcome::ProtocolFailure(error) => ToolOutput::failure(prepare_model_output(
            prefixed_name,
            protocol_diagnostic(&error),
            max_bytes,
        )),
    }
}

pub(crate) fn restart_failed_output(server: &str, tool: &str, failure: &str) -> String {
    let mut error = Map::new();
    error.insert("kind".to_owned(), Value::from("server_restart_failed"));
    error.insert(
        "message".to_owned(),
        Value::from(format!(
            "MCP server stopped and could not be restarted: {failure}"
        )),
    );
    let mut object = Map::new();
    object.insert("server".to_owned(), Value::from(server));
    object.insert("tool".to_owned(), Value::from(tool));
    object.insert("error".to_owned(), Value::Object(error));
    Value::Object(object).to_string()
}

pub(crate) fn protocol_diagnostic(error: &RpcError) -> String {
    let mut text = format!(
        "MCP protocol error {}: {}",
        error.code,
        sanitize(&error.message)
    );
    if let Some(data) = &error.data {
        text.push_str("; data=");
        text.push_str(&sanitize(&data.to_string()));
    }
    text
}

fn project_media_for_text(content: &mut Value) {
    match content {
        Value::Array(items) => items.iter_mut().for_each(project_media_block),
        other => project_media_block(other),
    }
}

fn project_media_block(item: &mut Value) {
    let Some(object) = item.as_object_mut() else {
        return;
    };
    match object.get("type").and_then(Value::as_str) {
        Some("image" | "audio") => {
            object.shift_remove("data");
            object.insert("delivery".to_owned(), Value::from(UNSUPPORTED_MEDIA));
        }
        Some("resource") => {
            if let Some(resource) = object.get_mut("resource").and_then(Value::as_object_mut)
                && resource.shift_remove("blob").is_some()
            {
                resource.insert("delivery".to_owned(), Value::from(UNSENT_BINARY_RESOURCE));
            }
        }
        _ => {}
    }
}

fn serialize_capped(server: &str, tool: &str, result: &Value, max_bytes: usize) -> String {
    let full = envelope(server, tool, project(result));
    if full.len() <= max_bytes {
        return full;
    }
    let mut collected = String::new();
    collect_text(result, &mut collected);
    let marker = format!(
        "\n... [mcp tool result truncated for {server}/{tool}: original {} bytes; cap is {max_bytes} bytes]\n",
        full.len()
    );
    let source = if collected.is_empty() {
        marker.as_str()
    } else {
        collected.as_str()
    };
    let truncated = |inner: &str| truncated_envelope(server, tool, inner, full.len(), max_bytes);
    let mut low = 0;
    let mut high = (source.len() + marker.len()).min(max_bytes);
    let mut best = None;
    while low <= high {
        let mid = low + (high - low) / 2;
        let candidate = truncated(&truncate_text(source, mid, &marker));
        if candidate.len() <= max_bytes {
            best = Some(candidate);
            low = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            high = mid - 1;
        }
    }
    best.unwrap_or_else(|| truncated(&marker))
}

fn truncate_text(text: &str, max_bytes: usize, marker: &str) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let prefix_len = text.floor_char_boundary(max_bytes.saturating_sub(marker.len()));
    format!("{}{marker}", &text[..prefix_len])
}

fn envelope(server: &str, tool: &str, result: Value) -> String {
    let mut object = Map::new();
    object.insert("server".to_owned(), Value::from(server));
    object.insert("tool".to_owned(), Value::from(tool));
    object.insert("result".to_owned(), result);
    Value::Object(object).to_string()
}

fn truncated_envelope(
    server: &str,
    tool: &str,
    text: &str,
    original_bytes: usize,
    cap_bytes: usize,
) -> String {
    let mut block = Map::new();
    block.insert("type".to_owned(), Value::from("text"));
    block.insert("text".to_owned(), Value::from(text));
    let mut result = Map::new();
    result.insert(
        "content".to_owned(),
        Value::Array(vec![Value::Object(block)]),
    );
    result.insert("truncated".to_owned(), Value::Bool(true));
    result.insert("original_bytes".to_owned(), Value::from(original_bytes));
    result.insert("cap_bytes".to_owned(), Value::from(cap_bytes));
    envelope(server, tool, Value::Object(result))
}

fn project(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(sanitize(text)),
        Value::Array(items) => Value::Array(items.iter().map(project).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), project(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn collect_text(value: &Value, out: &mut String) {
    match value {
        Value::String(text) => {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&sanitize(text));
        }
        Value::Array(items) => items.iter().for_each(|item| collect_text(item, out)),
        Value::Object(object) => object
            .iter()
            .filter(|(key, _)| TEXT_FIELDS.contains(&key.as_str()))
            .for_each(|(_, value)| collect_text(value, out)),
        _ => {}
    }
}

fn sanitize(text: &str) -> String {
    sanitize_model_text_owned(text.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use ofx_contract::ToolResultStatus;
    use serde_json::json;

    use super::*;
    use crate::features::tools::ToolCallResult;

    fn complete(result: Value, is_error: bool) -> ToolCallOutcome {
        ToolCallOutcome::Complete(ToolCallResult {
            content: Vec::new(),
            is_error,
            structured_content: None,
            result,
        })
    }

    #[test]
    fn results_are_wrapped_with_their_server_and_tool() {
        let output = model_output(
            "docs",
            "search",
            "mcp_docs_search",
            complete(json!({"content":[{"type":"text","text":"found"}]}), false),
            1024,
        );
        assert_eq!(output.status, ToolResultStatus::Success);
        assert_eq!(
            output.content,
            r#"{"server":"docs","tool":"search","result":{"content":[{"type":"text","text":"found"}]}}"#
        );
        let failed = model_output(
            "docs",
            "search",
            "mcp_docs_search",
            complete(json!({"content":[],"isError":true}), true),
            1024,
        );
        assert_eq!(failed.status, ToolResultStatus::Failure);
    }

    #[test]
    fn media_is_described_instead_of_sent() {
        let output = model_output(
            "s",
            "t",
            "mcp_s_t",
            complete(
                json!({"content":[
                    {"type":"image","data":"aGk=","mimeType":"image/png"},
                    {"type":"audio","data":"aGk=","mimeType":"audio/wav"},
                    {"type":"resource","resource":{"uri":"file:///a","blob":"aGk="}}
                ]}),
                false,
            ),
            4096,
        );
        assert!(!output.content.contains("aGk="));
        assert_eq!(output.content.matches(UNSUPPORTED_MEDIA).count(), 2);
        assert!(output.content.contains(UNSENT_BINARY_RESOURCE));
    }

    #[test]
    fn oversized_results_keep_their_text_within_the_cap() {
        let text = "x".repeat(4096);
        let output = model_output(
            "s",
            "t",
            "mcp_s_t",
            complete(json!({"content":[{"type":"text","text":text}]}), false),
            512,
        );
        assert!(output.content.len() <= 512);
        let parsed: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(parsed["result"]["truncated"], true);
        assert_eq!(parsed["result"]["cap_bytes"], 512);
        assert!(
            parsed["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("[mcp tool result truncated for s/t")
        );
    }

    #[test]
    fn protocol_errors_name_their_code_message_and_data() {
        let output = model_output(
            "s",
            "t",
            "mcp_s_t",
            ToolCallOutcome::ProtocolFailure(RpcError {
                code: -32_602,
                message: "bad\0input".to_owned(),
                data: Some(json!({"field":"q"})),
            }),
            1024,
        );
        assert_eq!(output.status, ToolResultStatus::Failure);
        assert!(output.content.starts_with("MCP protocol error -32602: "));
        assert!(output.content.ends_with(r#"; data={"field":"q"}"#));
    }
}
