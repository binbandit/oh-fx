use serde_json::{Value, json};

const STREAM_ID: &str = "chatcmpl-testkit";
const STREAM_MODEL: &str = "testkit-model";

fn chunk(choices: &Value) -> String {
    json!({
        "id": STREAM_ID,
        "object": "chat.completion.chunk",
        "model": STREAM_MODEL,
        "choices": choices,
    })
    .to_string()
}

fn finish(reason: &str) -> String {
    chunk(&json!([{"index": 0, "delta": {}, "finish_reason": reason}]))
}

fn usage_trailer() -> String {
    json!({
        "id": STREAM_ID,
        "object": "chat.completion.chunk",
        "model": STREAM_MODEL,
        "choices": [],
        "usage": {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15},
    })
    .to_string()
}

pub fn chat_text_events(deltas: &[&str]) -> Vec<String> {
    let mut events = vec![chunk(&json!([{
        "index": 0,
        "delta": {"role": "assistant", "content": ""},
        "finish_reason": null,
    }]))];
    events.extend(deltas.iter().map(|text| {
        chunk(&json!([{"index": 0, "delta": {"content": text}, "finish_reason": null}]))
    }));
    events.push(finish("stop"));
    events.push(usage_trailer());
    events.push("[DONE]".to_owned());
    events
}

pub fn chat_tool_call_events(call_id: &str, name: &str, arguments: &str) -> Vec<String> {
    let call = json!([{
        "index": 0,
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": arguments},
    }]);
    vec![
        chunk(&json!([{
            "index": 0,
            "delta": {"role": "assistant", "tool_calls": call},
            "finish_reason": null,
        }])),
        finish("tool_calls"),
        usage_trailer(),
        "[DONE]".to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_streams_end_with_finish_usage_and_done() {
        let events = chat_text_events(&["Hel", "lo"]);
        assert_eq!(events.len(), 6);
        assert!(events[1].contains(r#""content":"Hel""#));
        assert!(events[3].contains(r#""finish_reason":"stop""#));
        assert!(events[4].contains(r#""choices":[]"#));
        assert_eq!(events[5], "[DONE]");
    }

    #[test]
    fn tool_call_streams_finish_with_tool_calls() {
        let events = chat_tool_call_events("call-1", "read_file", r#"{"path":"x"}"#);
        assert!(events[0].contains(r#""name":"read_file""#));
        assert!(events[1].contains(r#""finish_reason":"tool_calls""#));
    }
}
