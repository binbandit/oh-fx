use std::fmt;

const MAX_SESSION_ID_BYTES: usize = 255;
const SESSIONS_V2_DIR: &str = "v2";

pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID_BYTES
        && id != "."
        && id != ".."
        && !id.eq_ignore_ascii_case(SESSIONS_V2_DIR)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCallId(String);

impl ToolCallId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TurnId(u64);

impl TurnId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

impl RequestId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_ids_display_their_provider_value() {
        let id = ToolCallId::new("call-1");
        assert_eq!(id.to_string(), "call-1");
        assert_eq!(id.as_str(), "call-1");
        assert_eq!(TurnId::new(3), TurnId::new(3));
        assert_ne!(RequestId::new(1), RequestId::new(2));
    }
}
