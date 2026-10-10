use std::fmt::Write;

use ofx_contract::{Json, Object, ProviderError, ProviderErrorKind};
use serde_json::{Map, Value};

pub(crate) const MAX_REPLAY_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_IDENTITY_BYTES: usize = 256;
const PART_BYTES: usize = 96;
const PROVIDER_NAMESPACE: &str = "gateway";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ReplayError {
    #[error("InvalidProviderState")]
    InvalidProviderState,
    #[error("ProviderStateTooLarge")]
    ProviderStateTooLarge,
}

impl From<ReplayError> for ProviderError {
    fn from(error: ReplayError) -> Self {
        Self::new(ProviderErrorKind::Protocol, error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Reasoning,
    ToolCall,
}

#[derive(Debug)]
struct Part {
    kind: Kind,
    id: String,
    text: String,
    offset: usize,
    length: usize,
    metadata: Option<Value>,
    has_provider_metadata: bool,
    ended: bool,
}

pub(crate) struct ReplayCall<'a> {
    pub(crate) id: &'a str,
    pub(crate) provisional_id: Option<&'a str>,
}

#[derive(Debug, Default)]
pub(crate) struct ReplayBuilder {
    parts: Vec<Part>,
    retained_bytes: usize,
    needed: bool,
}

impl ReplayBuilder {
    fn reserve(&mut self, bytes: usize) -> Result<(), ReplayError> {
        if bytes > MAX_REPLAY_BYTES - self.retained_bytes {
            return Err(ReplayError::ProviderStateTooLarge);
        }
        self.retained_bytes += bytes;
        Ok(())
    }

    fn find(&self, kind: Kind, id: &str, starts_segment: bool) -> Option<usize> {
        let matches = |index: &usize| {
            let part = &self.parts[*index];
            part.kind == kind && part.id == id
        };
        if kind == Kind::ToolCall {
            return (0..self.parts.len()).find(matches);
        }
        let index = (0..self.parts.len()).rev().find(matches)?;
        (!(starts_segment && self.parts[index].ended)).then_some(index)
    }

    pub(crate) fn observe(
        &mut self,
        event: &str,
        fields: &Object<'_>,
        content_offset: usize,
    ) -> Result<(), ReplayError> {
        let kind = if event.starts_with("reasoning-") {
            Kind::Reasoning
        } else if event.starts_with("text-") {
            Kind::Text
        } else if event.starts_with("tool-input-") || event == "tool-call" {
            Kind::ToolCall
        } else {
            return Ok(());
        };
        if kind == Kind::Reasoning
            && !matches!(
                event,
                "reasoning-start" | "reasoning-delta" | "reasoning-end"
            )
        {
            return Ok(());
        }
        let key = if event == "tool-call" {
            "toolCallId"
        } else {
            "id"
        };
        let id = fields.get(key).and_then(Json::as_str).unwrap_or("");
        if kind == Kind::ToolCall && (id.is_empty() || id.len() > MAX_IDENTITY_BYTES) {
            return Ok(());
        }
        if id.len() > MAX_IDENTITY_BYTES {
            return Err(ReplayError::ProviderStateTooLarge);
        }
        let starts_segment = matches!(event, "text-start" | "reasoning-start");
        let index = if let Some(index) = self.find(kind, id, starts_segment) {
            index
        } else {
            self.reserve(PART_BYTES + id.len())?;
            self.parts.push(Part {
                kind,
                id: id.to_owned(),
                text: String::new(),
                offset: content_offset,
                length: 0,
                metadata: None,
                has_provider_metadata: false,
                ended: false,
            });
            self.parts.len() - 1
        };
        if kind == Kind::Reasoning {
            self.needed = true;
        }
        if event.ends_with("-delta") && kind != Kind::ToolCall {
            let Some(delta) = fields.get("delta") else {
                return Ok(());
            };
            let delta = delta.as_str().ok_or(ReplayError::InvalidProviderState)?;
            if self.parts[index].ended {
                return Err(ReplayError::InvalidProviderState);
            }
            if kind == Kind::Reasoning {
                self.reserve(delta.len())?;
                self.parts[index].text.push_str(delta);
            } else {
                let part = &mut self.parts[index];
                let end = part
                    .offset
                    .checked_add(part.length)
                    .ok_or(ReplayError::ProviderStateTooLarge)?;
                if content_offset != end {
                    return Err(ReplayError::InvalidProviderState);
                }
                part.length = part
                    .length
                    .checked_add(delta.len())
                    .ok_or(ReplayError::ProviderStateTooLarge)?;
            }
        }
        if event.ends_with("-end") || event == "tool-call" {
            self.parts[index].ended = true;
        }
        if let Some(metadata) = fields.get("providerMetadata") {
            self.observe_metadata(index, metadata)?;
        }
        Ok(())
    }

    fn observe_metadata(&mut self, index: usize, metadata: &Json<'_>) -> Result<(), ReplayError> {
        let fields = metadata
            .as_object()
            .ok_or(ReplayError::InvalidProviderState)?;
        if fields
            .iter()
            .any(|(_, options)| options.as_object().is_none())
        {
            return Err(ReplayError::InvalidProviderState);
        }
        if fields.iter().any(|(key, _)| key != PROVIDER_NAMESPACE) {
            self.needed = true;
            self.parts[index].has_provider_metadata = true;
        }
        let part = &self.parts[index];
        if part.has_provider_metadata && part.id.is_empty() {
            return Err(ReplayError::InvalidProviderState);
        }
        let previous = part.metadata.as_ref().map_or(0, serialized_len);
        let mut merged = part
            .metadata
            .clone()
            .unwrap_or_else(|| Value::Object(Map::new()));
        merge(&mut merged, metadata.clone().into_value());
        let next = serialized_len(&merged);
        if next > previous {
            self.reserve(next - previous)?;
        } else {
            self.retained_bytes -= previous - next;
        }
        self.parts[index].metadata = Some(merged);
        Ok(())
    }

    pub(crate) fn finish(
        &self,
        content: &str,
        calls: &[ReplayCall<'_>],
    ) -> Result<Option<String>, ReplayError> {
        if !self.needed {
            return Ok(None);
        }
        let mut out = String::from("[");
        let mut emitted = false;
        for (index, part) in self.parts.iter().enumerate() {
            let mut metadata = part.metadata.clone();
            let mut canonical = None;
            if part.kind == Kind::ToolCall {
                let Some(call) = calls.iter().find(|call| matches_call(part, call)) else {
                    if part.has_provider_metadata {
                        return Err(ReplayError::InvalidProviderState);
                    }
                    continue;
                };
                if self.parts[..index]
                    .iter()
                    .any(|prior| matches_call(prior, call))
                {
                    continue;
                }
                for later in &self.parts[index + 1..] {
                    if let (true, Some(next)) = (matches_call(later, call), &later.metadata) {
                        let target = metadata.get_or_insert_with(|| Value::Object(Map::new()));
                        merge(target, next.clone());
                    }
                }
                canonical = Some(call.id);
            }
            if emitted {
                out.push(',');
            }
            match part.kind {
                Kind::Reasoning => {
                    if !part.ended && part.metadata.is_some() {
                        return Err(ReplayError::InvalidProviderState);
                    }
                    out.push_str("{\"type\":\"reasoning\",\"text\":");
                    out.push_str(&Value::from(part.text.as_str()).to_string());
                }
                Kind::Text => {
                    if part.offset > content.len() || part.length > content.len() - part.offset {
                        return Err(ReplayError::InvalidProviderState);
                    }
                    let _ = write!(
                        out,
                        "{{\"type\":\"text\",\"offset\":{},\"length\":{}",
                        part.offset, part.length
                    );
                }
                Kind::ToolCall => {
                    out.push_str("{\"type\":\"tool-call\",\"toolCallId\":");
                    out.push_str(&Value::from(canonical.unwrap_or_default()).to_string());
                }
            }
            if let Some(value) = &metadata {
                out.push_str(",\"providerOptions\":");
                out.push_str(&value.to_string());
            }
            out.push('}');
            emitted = true;
            if out.len() > MAX_REPLAY_BYTES {
                return Err(ReplayError::ProviderStateTooLarge);
            }
        }
        out.push(']');
        if out.len() > MAX_REPLAY_BYTES {
            return Err(ReplayError::ProviderStateTooLarge);
        }
        Ok(Some(out))
    }
}

fn matches_call(part: &Part, call: &ReplayCall<'_>) -> bool {
    part.kind == Kind::ToolCall
        && (part.id == call.id || call.provisional_id.is_some_and(|id| part.id == id))
}

fn serialized_len(value: &Value) -> usize {
    value.to_string().len()
}

fn merge(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                match target.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, source) => *target = source,
    }
}

#[cfg(test)]
mod tests;
