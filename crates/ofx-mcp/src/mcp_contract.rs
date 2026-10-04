use std::path::PathBuf;

use serde_json::Value;

use crate::error::McpError;

pub const DEFAULT_STARTUP_TIMEOUT_MS: u32 = 30_000;
pub const DEFAULT_OPERATION_TIMEOUT_MS: u32 = 60_000;
pub const DEFAULT_RESTART_LIMIT: u8 = 1;
pub(crate) const MAX_PROFILE_CONFIG_WARNING_KEY_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileConfigWarningCause {
    IgnoredMcpServersAlias,
    SuspiciousServerKey,
    SuspiciousKeyScanIndeterminate,
}

impl ProfileConfigWarningCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IgnoredMcpServersAlias => "ignored_mcp_servers_alias",
            Self::SuspiciousServerKey => "suspicious_server_key",
            Self::SuspiciousKeyScanIndeterminate => "suspicious_key_scan_indeterminate",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileConfigWarning {
    pub cause: ProfileConfigWarningCause,
    pub key: Option<String>,
    pub additional_matches: usize,
}

impl ProfileConfigWarning {
    pub fn new(
        cause: ProfileConfigWarningCause,
        key: Option<&str>,
        additional_matches: usize,
    ) -> Self {
        Self {
            cause,
            key: key
                .map(|value| bounded_prefix(value, MAX_PROFILE_CONFIG_WARNING_KEY_BYTES))
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            additional_matches,
        }
    }
}

fn bounded_prefix(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportType {
    Stdio,
    Http,
    Sse,
}

impl TransportType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Sse => "sse",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    Profile,
    Acp,
    Workspace,
}

impl ConfigSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Acp => "acp",
            Self::Workspace => "workspace",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    Profile,
    AcpSession,
    Workspace,
}

impl ConfigScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::AcpSession => "acp_session",
            Self::Workspace => "workspace",
        }
    }
}

pub(crate) fn source_allows_scope(source: ConfigSource, scope: ConfigScope) -> bool {
    match source {
        ConfigSource::Profile => scope == ConfigScope::Profile,
        ConfigSource::Acp => scope == ConfigScope::AcpSession,
        ConfigSource::Workspace => scope == ConfigScope::Workspace,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAdmission {
    Pending,
    Approved,
    Rejected,
}

impl WorkspaceAdmission {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

pub(crate) fn source_allows_workspace_admission(
    source: ConfigSource,
    admission: Option<WorkspaceAdmission>,
) -> bool {
    match source {
        ConfigSource::Workspace => admission.is_some(),
        ConfigSource::Profile | ConfigSource::Acp => admission.is_none(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpHeaderEnv {
    pub name: String,
    pub env: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpAuthConfig {
    pub resource: Option<String>,
    pub issuer: Option<String>,
    pub client_id: Option<String>,
    pub client_secret_env: Option<String>,
    pub client_metadata_url: Option<String>,
    pub scopes: Vec<String>,
    pub scopes_configured: bool,
    pub callback_port: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("McpInvalidServerConfig")]
pub struct InvalidServerConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub source: ConfigSource,
    pub scope: ConfigScope,
    pub required: bool,
    pub transport: TransportType,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub url: Option<String>,
    pub env: Vec<EnvVar>,
    pub headers: Vec<HttpHeader>,
    pub header_env: Vec<HttpHeaderEnv>,
    pub bearer_token_env: Option<String>,
    pub auth: Option<McpAuthConfig>,
    pub allow_stored_credentials: bool,
    pub enabled: bool,
    pub workspace_admission: Option<WorkspaceAdmission>,
    pub startup_timeout_ms: u32,
    pub operation_timeout_ms: u32,
    pub restart_limit: u8,
}

impl McpServerConfig {
    pub fn new(name: impl Into<String>, transport: TransportType) -> Self {
        Self {
            name: name.into(),
            source: ConfigSource::Profile,
            scope: ConfigScope::Profile,
            required: false,
            transport,
            command: None,
            args: Vec::new(),
            cwd: None,
            url: None,
            env: Vec::new(),
            headers: Vec::new(),
            header_env: Vec::new(),
            bearer_token_env: None,
            auth: None,
            allow_stored_credentials: false,
            enabled: true,
            workspace_admission: None,
            startup_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS,
            operation_timeout_ms: DEFAULT_OPERATION_TIMEOUT_MS,
            restart_limit: DEFAULT_RESTART_LIMIT,
        }
    }

    pub fn stdio(name: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            command: Some(command.into()),
            args,
            ..Self::new(name, TransportType::Stdio)
        }
    }

    pub fn remote(
        name: impl Into<String>,
        transport: TransportType,
        url: impl Into<String>,
    ) -> Self {
        Self {
            url: Some(url.into()),
            ..Self::new(name, transport)
        }
    }

    pub fn stdio_command(&self) -> Result<&str, InvalidServerConfig> {
        if self.transport != TransportType::Stdio {
            return Err(InvalidServerConfig);
        }
        self.command
            .as_deref()
            .filter(|command| !command.is_empty())
            .ok_or(InvalidServerConfig)
    }

    pub fn remote_url(&self) -> Result<&str, InvalidServerConfig> {
        if self.transport == TransportType::Stdio {
            return Err(InvalidServerConfig);
        }
        self.url
            .as_deref()
            .filter(|url| !url.is_empty())
            .ok_or(InvalidServerConfig)
    }
}

pub(crate) fn validate_json_rpc_response_envelope(value: &Value) -> Result<(), McpError> {
    let object = value.as_object().ok_or(McpError::McpInvalidJson)?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpError::McpInvalidJson);
    }
    if object.contains_key("result") == object.contains_key("error") {
        return Err(McpError::McpInvalidJson);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_rpc_response_envelopes_require_version_2_0_and_one_payload_member() {
        let cases = [
            (r#"{"jsonrpc":"2.0","id":1,"result":null}"#, true),
            (r#"{"jsonrpc":"2.0","id":1,"error":null}"#, true),
            (r#"{"id":1,"result":null}"#, false),
            (r#"{"jsonrpc":"1.0","id":1,"result":null}"#, false),
            (r#"{"jsonrpc":"2.0","id":1}"#, false),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":null,"error":null}"#,
                false,
            ),
        ];
        for (json, valid) in cases {
            let value: Value = serde_json::from_str(json).unwrap();
            let expected = if valid {
                Ok(())
            } else {
                Err(McpError::McpInvalidJson)
            };
            assert_eq!(
                validate_json_rpc_response_envelope(&value),
                expected,
                "{json}"
            );
        }
    }

    #[test]
    fn mcp_default_startup_timeout_allows_thirty_second_cold_starts() {
        assert_eq!(DEFAULT_STARTUP_TIMEOUT_MS, 30_000);
    }

    #[test]
    fn mcp_server_configuration_exposes_only_the_active_transport_target() {
        let stdio = McpServerConfig::stdio("stdio", "node", Vec::new());
        assert_eq!(stdio.stdio_command(), Ok("node"));
        assert_eq!(stdio.remote_url(), Err(InvalidServerConfig));

        let remote =
            McpServerConfig::remote("remote", TransportType::Http, "https://example.test/mcp");
        assert_eq!(remote.remote_url(), Ok("https://example.test/mcp"));
        assert_eq!(remote.stdio_command(), Err(InvalidServerConfig));

        let missing_stdio = McpServerConfig::new("missing", TransportType::Stdio);
        assert_eq!(missing_stdio.stdio_command(), Err(InvalidServerConfig));

        let empty_remote = McpServerConfig::remote("empty", TransportType::Sse, "");
        assert_eq!(empty_remote.remote_url(), Err(InvalidServerConfig));
    }

    #[test]
    fn profile_config_warning_owns_a_bounded_key_inline() {
        let warning = ProfileConfigWarning::new(
            ProfileConfigWarningCause::SuspiciousServerKey,
            Some("MCP-Servers"),
            2,
        );
        assert_eq!(warning.key.as_deref(), Some("MCP-Servers"));
        assert_eq!(warning.additional_matches, 2);
        assert_eq!(
            ProfileConfigWarning::new(
                ProfileConfigWarningCause::SuspiciousKeyScanIndeterminate,
                None,
                0
            )
            .key,
            None
        );
        let long = "k".repeat(200);
        let bounded = ProfileConfigWarning::new(
            ProfileConfigWarningCause::SuspiciousServerKey,
            Some(&long),
            0,
        );
        assert_eq!(bounded.key.map(|key| key.len()), Some(128));
    }

    #[test]
    fn mcp_configuration_sources_admit_only_their_product_scope() {
        let sources = [
            ConfigSource::Profile,
            ConfigSource::Acp,
            ConfigSource::Workspace,
        ];
        let scopes = [
            ConfigScope::Profile,
            ConfigScope::AcpSession,
            ConfigScope::Workspace,
        ];
        for source in sources {
            for scope in scopes {
                let expected = match source {
                    ConfigSource::Profile => scope == ConfigScope::Profile,
                    ConfigSource::Acp => scope == ConfigScope::AcpSession,
                    ConfigSource::Workspace => scope == ConfigScope::Workspace,
                };
                assert_eq!(source_allows_scope(source, scope), expected);
            }
        }
    }

    #[test]
    fn workspace_admission_is_present_exactly_for_workspace_source() {
        let admissions = [
            None,
            Some(WorkspaceAdmission::Pending),
            Some(WorkspaceAdmission::Approved),
            Some(WorkspaceAdmission::Rejected),
        ];
        for source in [
            ConfigSource::Profile,
            ConfigSource::Acp,
            ConfigSource::Workspace,
        ] {
            for admission in admissions {
                let expected = if source == ConfigSource::Workspace {
                    admission.is_some()
                } else {
                    admission.is_none()
                };
                assert_eq!(
                    source_allows_workspace_admission(source, admission),
                    expected
                );
            }
        }
    }
}
