use std::collections::{HashMap, HashSet};

use crate::error::McpError;

const MAX_ALIASES: usize = 64 * 1024;
const MAX_NAME_LEN: usize = 64;
const MAX_TOOL_TAGS: usize = 16;

#[derive(Debug, Default)]
pub(crate) struct ToolNames {
    by_identity: HashMap<(String, String), String>,
    aliases: HashSet<String>,
}

impl ToolNames {
    pub(crate) fn name(
        &mut self,
        reserved: &[String],
        server: &str,
        tool: &str,
    ) -> Result<String, McpError> {
        let identity = (server.to_owned(), tool.to_owned());
        if let Some(existing) = self.by_identity.get(&identity) {
            return Ok(existing.clone());
        }
        if self.by_identity.len() >= MAX_ALIASES {
            return Err(McpError::McpToolNameLimitExceeded);
        }
        let base = base_tool_name(server, tool);
        let mut suffix = None;
        loop {
            let candidate = candidate_with_suffix(&base, suffix);
            if reserved.contains(&candidate) || self.aliases.contains(&candidate) {
                suffix = Some(suffix.map_or(2, |value| value + 1));
                continue;
            }
            self.aliases.insert(candidate.clone());
            self.by_identity.insert(identity, candidate.clone());
            return Ok(candidate);
        }
    }
}

pub(crate) fn tags_for(server: &str, tool: &str) -> Vec<String> {
    let mut tags = Vec::new();
    push_tag(&mut tags, "mcp");
    for text in [server, tool] {
        for token in
            text.split(|character: char| !u8::try_from(character).is_ok_and(is_identifier_byte))
        {
            push_tag(&mut tags, token);
        }
    }
    tags
}

fn push_tag(tags: &mut Vec<String>, raw: &str) {
    if raw.is_empty() || tags.len() >= MAX_TOOL_TAGS {
        return;
    }
    let normalized = raw.to_ascii_lowercase();
    if !tags.contains(&normalized) {
        tags.push(normalized);
    }
}

pub(crate) fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn base_tool_name(server: &str, tool: &str) -> String {
    let mut name = String::from("mcp_");
    push_segment(&mut name, server, "server");
    name.push('_');
    push_segment(&mut name, tool, "tool");
    name
}

fn push_segment(name: &mut String, segment: &str, fallback: &str) {
    if segment.is_empty() {
        name.push_str(fallback);
        return;
    }
    name.extend(segment.bytes().map(|byte| {
        if is_identifier_byte(byte) {
            char::from(byte)
        } else {
            '_'
        }
    }));
}

fn candidate_with_suffix(base: &str, suffix: Option<usize>) -> String {
    match suffix {
        Some(index) => {
            let suffix = format!("_{index}");
            let prefix_len = base.len().min(MAX_NAME_LEN - suffix.len());
            format!("{}{suffix}", base.get(..prefix_len).unwrap_or_default())
        }
        None => base
            .get(..base.len().min(MAX_NAME_LEN))
            .unwrap_or_default()
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_tool_aliases_survive_replacement_and_cannot_retarget_a_collision() {
        let mut names = ToolNames::default();
        let first = names.name(&[], "a/b", "c").unwrap();
        let collision = names.name(&[], "a_b", "c").unwrap();
        assert_ne!(first, collision);
        assert_eq!(names.name(&[], "a/b", "c").unwrap(), first);
        assert_eq!(names.name(&[], "a_b", "c").unwrap(), collision);
        assert_eq!(first, "mcp_a_b_c");
        assert_eq!(collision, "mcp_a_b_c_2");
    }

    #[test]
    fn tags_are_the_lowercase_identifier_runs_of_the_server_and_tool_names() {
        assert_eq!(
            tags_for("Data.Dog", "list_Monitors data"),
            ["mcp", "data", "dog", "list_monitors"]
        );
        assert_eq!(tags_for("mcp", "MCP"), ["mcp"]);
        assert_eq!(tags_for("é-x", "y€z"), ["mcp", "-x", "y", "z"]);
        let many = (0..20).map(|index| format!("t{index}")).collect::<Vec<_>>();
        let tags = tags_for("server", &many.join(" "));
        assert_eq!(tags.len(), 16);
        assert_eq!(tags[..3], ["mcp", "server", "t0"]);
        assert_eq!(tags[15], "t13");
    }

    #[test]
    fn names_avoid_built_in_tools_and_stay_within_sixty_four_bytes() {
        let mut names = ToolNames::default();
        let reserved = vec!["mcp_files_read".to_owned()];
        assert_eq!(
            names.name(&reserved, "files", "read").unwrap(),
            "mcp_files_read_2"
        );
        assert_eq!(names.name(&[], "", "").unwrap(), "mcp_server_tool");
        let long = names.name(&[], "server", &"x".repeat(100)).unwrap();
        assert_eq!(long.len(), 64);
        let long_collision = names.name(&[], "server", &"x".repeat(101)).unwrap();
        assert_eq!(long_collision.len(), 64);
        assert!(long_collision.ends_with("_2"));
        assert_eq!(
            names.name(&[], "dé jà", "vu.tool").unwrap(),
            "mcp_d___j___vu_tool"
        );
    }
}
