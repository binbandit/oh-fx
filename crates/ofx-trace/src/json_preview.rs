use std::fmt::Write as _;

use serde_json::Value;

const MAX_DEPTH: usize = 2;
const MAX_FIELDS: usize = 6;

pub fn keyless_json_preview(text: &str) -> String {
    if text.is_empty() {
        return "<empty>".to_owned();
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return format!("<invalid-json bytes={}>", text.len());
    };
    let mut shown = String::new();
    write_shape(&mut shown, &value, 0);
    shown
}

fn write_shape(shown: &mut String, value: &Value, depth: usize) {
    match value {
        Value::Object(object) => {
            let _ = write!(shown, "<object_fields={}", object.len());
            if depth < MAX_DEPTH && !object.is_empty() {
                shown.push_str(" values=[");
                for (written, field) in object.values().enumerate() {
                    if written > 0 {
                        shown.push(',');
                    }
                    if written >= MAX_FIELDS {
                        shown.push_str("...");
                        break;
                    }
                    write_shape(shown, field, depth + 1);
                }
                shown.push(']');
            }
            shown.push('>');
        }
        Value::Array(items) => {
            let _ = write!(shown, "<array_len={}>", items.len());
        }
        Value::String(text) => {
            let _ = write!(shown, "<string_bytes={}>", text.len());
        }
        Value::Number(_) => shown.push_str("<number>"),
        Value::Bool(_) => shown.push_str("<bool>"),
        Value::Null => shown.push_str("<null>"),
    }
}

#[cfg(test)]
mod tests;
