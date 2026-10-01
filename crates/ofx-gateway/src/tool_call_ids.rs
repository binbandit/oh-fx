use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use ofx_contract::ChatMessage;
use sha2::{Digest, Sha256};

const MAX_ID_BYTES: usize = 64;
const ALIAS_DIGEST_BYTES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectionError {
    InvalidToolCallId,
    ToolCallIdMappingExhausted,
    ProtectedToolCallId,
}

#[derive(Debug, Default)]
pub(crate) struct Projection {
    aliases: HashMap<String, String>,
}

impl Projection {
    pub(crate) fn new(messages: &[ChatMessage]) -> Result<Self, ProjectionError> {
        Self::protecting(messages, &[])
    }

    pub(crate) fn protecting(
        messages: &[ChatMessage],
        replayed: &[bool],
    ) -> Result<Self, ProjectionError> {
        let mut known = HashSet::new();
        let mut needed = false;
        for id in all_ids(messages) {
            if id.is_empty() {
                return Err(ProjectionError::InvalidToolCallId);
            }
            needed = needed || !portable(id);
            known.insert(id.to_owned());
        }
        let mut projection = Self::default();
        if !needed {
            return Ok(projection);
        }
        let is_replayed = |index: usize| replayed.get(index).copied().unwrap_or(false);
        let protected: HashSet<&str> = messages
            .iter()
            .enumerate()
            .filter(|(index, _)| is_replayed(*index))
            .flat_map(|(_, message)| call_ids(std::slice::from_ref(message)))
            .collect();
        let opaque_history = replayed.contains(&true);
        for (index, message) in messages.iter().enumerate() {
            let ChatMessage::Assistant { tool_calls, .. } = message else {
                continue;
            };
            for call in tool_calls {
                let id = call.id.as_str();
                if portable(projection.resolve(id)) || is_replayed(index) {
                    continue;
                }
                if protected.contains(id) {
                    return Err(ProjectionError::ProtectedToolCallId);
                }
                let alias = next_alias(id, &known, opaque_history)?;
                known.insert(alias.clone());
                projection.aliases.insert(id.to_owned(), alias);
            }
        }
        for message in messages {
            if let ChatMessage::Tool { call_id, .. } = message
                && !protected.contains(call_id.as_str())
                && !portable(projection.resolve(call_id.as_str()))
            {
                return Err(ProjectionError::InvalidToolCallId);
            }
        }
        Ok(projection)
    }

    pub(crate) fn resolve<'a>(&'a self, id: &'a str) -> &'a str {
        self.aliases.get(id).map_or(id, String::as_str)
    }
}

fn call_ids(messages: &[ChatMessage]) -> impl Iterator<Item = &str> {
    messages
        .iter()
        .flat_map(|message| match message {
            ChatMessage::Assistant { tool_calls, .. } => tool_calls.as_slice(),
            _ => &[],
        })
        .map(|call| call.id.as_str())
}

fn all_ids(messages: &[ChatMessage]) -> impl Iterator<Item = &str> {
    let results = messages.iter().filter_map(|message| match message {
        ChatMessage::Tool { call_id, .. } => Some(call_id.as_str()),
        _ => None,
    });
    call_ids(messages).chain(results)
}

fn next_alias(
    id: &str,
    known: &HashSet<String>,
    opaque_history: bool,
) -> Result<String, ProjectionError> {
    let digest = Sha256::digest(id.as_bytes());
    let mut hex = String::with_capacity(ALIAS_DIGEST_BYTES * 2);
    for byte in &digest[..ALIAS_DIGEST_BYTES] {
        let _ = write!(hex, "{byte:02x}");
    }
    for attempt in 0..=known.len() {
        let candidate = format!("fx_{hex}_{attempt}");
        if !known.contains(&candidate) {
            return Ok(candidate);
        }
        if opaque_history {
            return Err(ProjectionError::ProtectedToolCallId);
        }
    }
    Err(ProjectionError::ToolCallIdMappingExhausted)
}

fn portable(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[cfg(test)]
mod tests {
    use ofx_contract::{ToolCall, ToolCallId, ToolResultStatus};

    use super::*;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(id),
            name: "read".to_owned(),
            arguments: "{}".to_owned(),
        }
    }

    fn assistant(calls: Vec<ToolCall>) -> ChatMessage {
        ChatMessage::Assistant {
            content: None,
            tool_calls: calls,
        }
    }

    fn result(id: &str, content: &str) -> ChatMessage {
        ChatMessage::Tool {
            call_id: ToolCallId::new(id),
            tool_name: "read".to_owned(),
            content: content.to_owned(),
            status: ToolResultStatus::Success,
        }
    }

    #[test]
    fn portable_tool_call_ids_use_an_allocation_free_identity_projection() {
        let long = "x".repeat(64);
        let messages = [assistant(vec![call("call_ABC-123"), call(&long)])];
        let projection = Projection::new(&messages).unwrap();
        assert!(projection.aliases.is_empty());
        assert_eq!(projection.resolve("call_ABC-123"), "call_ABC-123");
        assert_eq!(projection.resolve(&long), long);
    }

    #[test]
    fn tool_call_id_projection_is_deterministic_and_preserves_source_ids() {
        let sources = [
            "functions.read:0".to_owned(),
            "functions/read:0".to_owned(),
            " ".to_owned(),
            "\t\n".to_owned(),
            "é".to_owned(),
            "x".repeat(65),
            "x".repeat(256),
        ];
        let messages = [assistant(
            sources.iter().map(|source| call(source)).collect(),
        )];
        let first = Projection::new(&messages).unwrap();
        let second = Projection::new(&messages).unwrap();
        for (index, source) in sources.iter().enumerate() {
            let id = first.resolve(source);
            assert!(portable(id), "{id}");
            assert!(id.starts_with("fx_") && id.len() == 45);
            assert_eq!(id, second.resolve(source));
            for prior in &sources[..index] {
                assert_ne!(id, first.resolve(prior));
            }
        }
    }

    #[test]
    fn tool_call_id_projection_avoids_aliases_reserved_by_portable_calls() {
        let source = "functions.read:0";
        let first = Projection::new(&[assistant(vec![call(source)])]).unwrap();
        let original_alias = first.resolve(source).to_owned();
        let second =
            Projection::new(&[assistant(vec![call(source), call(&original_alias)])]).unwrap();
        assert_eq!(second.resolve(&original_alias), original_alias);
        assert_ne!(second.resolve(source), original_alias);
        assert!(portable(second.resolve(source)));
    }

    #[test]
    fn tool_call_id_projection_reuses_aliases_across_settled_steps() {
        let messages = [
            assistant(vec![call("call:0")]),
            result("call:0", "one"),
            assistant(vec![call("call:0")]),
            result("call:0", "two"),
        ];
        let projection = Projection::new(&messages).unwrap();
        assert_eq!(projection.aliases.len(), 1);
        let alias = projection.resolve("call:0");
        assert!(alias.starts_with("fx_"));
    }

    #[test]
    fn tool_call_id_projection_never_rewrites_protected_identities() {
        let replayed = [assistant(vec![call("native:0")]), result("native:0", "")];
        let projection = Projection::protecting(&replayed, &[true, false]).unwrap();
        assert_eq!(projection.resolve("native:0"), "native:0");
        let reused = [
            assistant(vec![call("native:0")]),
            assistant(vec![call("native:0")]),
        ];
        assert_eq!(
            Projection::protecting(&reused, &[false, true]).unwrap_err(),
            ProjectionError::ProtectedToolCallId
        );
    }

    #[test]
    fn tool_call_id_projection_rejects_unpaired_nonportable_result_ids() {
        assert_eq!(
            Projection::new(&[result("missing:0", "")]).unwrap_err(),
            ProjectionError::InvalidToolCallId
        );
    }
}
