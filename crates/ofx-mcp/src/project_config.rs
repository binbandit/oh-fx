use ofx_text::encode_terminal_safe;
use serde_json::{Map, Value};

use crate::mcp_contract::{
    ConfigScope, ConfigSource, DEFAULT_OPERATION_TIMEOUT_MS, DEFAULT_RESTART_LIMIT,
    DEFAULT_STARTUP_TIMEOUT_MS, EnvVar, HttpHeader, HttpHeaderEnv,
    MAX_PROFILE_CONFIG_WARNING_KEY_BYTES, McpAuthConfig, McpServerConfig, ProfileConfigWarning,
    ProfileConfigWarningCause, TransportType, WorkspaceAdmission, source_allows_scope,
    source_allows_workspace_admission,
};
use crate::streamable_http::{validate_endpoint, validate_static_headers};
use crate::uri::Uri;

pub const ENABLED_SERVERS_KEY: &str = "enabledMcpjsonServers";
pub const DISABLED_SERVERS_KEY: &str = "disabledMcpjsonServers";
pub const ENABLE_ALL_KEY: &str = "enableAllProjectMcpServers";

const MAX_PROFILE_ROOT_SCAN_ENTRIES: usize = 64;
const MAX_PROFILE_CHILD_SCAN_ENTRIES: usize = 64;
const MAX_RENDERED_SERVER_NAME_BYTES: usize = 160;
const MAX_RENDERED_ENVIRONMENT_NAME_BYTES: usize = 128;
const MAX_EXPANDED_WORKSPACE_VALUE_BYTES: usize = 1024 * 1024;
const MAX_EXPANDED_WORKSPACE_TOTAL_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum McpConfigError {
    #[error("McpConfigInvalidJson")]
    McpConfigInvalidJson,
    #[error("McpConfigRootMustBeObject")]
    McpConfigRootMustBeObject,
    #[error("McpConfigServersMustBeObject")]
    McpConfigServersMustBeObject,
    #[error("McpConfigServerMustBeObject")]
    McpConfigServerMustBeObject,
    #[error("McpConfigPolicyInvalid")]
    McpConfigPolicyInvalid,
    #[error("McpConfigInvalidType")]
    McpConfigInvalidType,
    #[error("McpConfigInvalidEnabled")]
    McpConfigInvalidEnabled,
    #[error("McpConfigInvalidRequired")]
    McpConfigInvalidRequired,
    #[error("McpConfigMissingUrl")]
    McpConfigMissingUrl,
    #[error("McpConfigInvalidUrl")]
    McpConfigInvalidUrl,
    #[error("McpConfigInvalidStartupTimeout")]
    McpConfigInvalidStartupTimeout,
    #[error("McpConfigInvalidOperationTimeout")]
    McpConfigInvalidOperationTimeout,
    #[error("McpConfigInvalidRestartLimit")]
    McpConfigInvalidRestartLimit,
    #[error("McpConfigInvalidCommand")]
    McpConfigInvalidCommand,
    #[error("McpConfigInvalidEnvironment")]
    McpConfigInvalidEnvironment,
    #[error("McpConfigInvalidHeaders")]
    McpConfigInvalidHeaders,
    #[error("McpConfigInvalidBearerEnvironment")]
    McpConfigInvalidBearerEnvironment,
    #[error("McpConfigInvalidOAuth")]
    McpConfigInvalidOAuth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("InvalidProjectMcpChoices")]
pub struct InvalidProjectMcpChoices;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceDiagnosticCause {
    InvalidJson,
    RootMustBeObject,
    ServersMustBeObject,
    InvalidEntry,
    MissingEnvironmentVariable,
    EnvironmentExpansionLimitExceeded,
    ApprovedRejectedOverlap,
}

impl WorkspaceDiagnosticCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::RootMustBeObject => "root_must_be_object",
            Self::ServersMustBeObject => "servers_must_be_object",
            Self::InvalidEntry => "invalid_entry",
            Self::MissingEnvironmentVariable => "missing_environment_variable",
            Self::EnvironmentExpansionLimitExceeded => "environment_expansion_limit_exceeded",
            Self::ApprovedRejectedOverlap => "approved_rejected_overlap",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceEnvironmentField {
    Command,
    Argument,
    Environment,
    HttpHeader,
}

impl WorkspaceEnvironmentField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Argument => "argument",
            Self::Environment => "environment",
            Self::HttpHeader => "http_header",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceDiagnostic {
    pub server_name: Option<String>,
    pub environment_variable: Option<String>,
    pub environment_field: Option<WorkspaceEnvironmentField>,
    pub cause: WorkspaceDiagnosticCause,
}

impl WorkspaceDiagnostic {
    pub(crate) fn new(cause: WorkspaceDiagnosticCause) -> Self {
        Self {
            server_name: None,
            environment_variable: None,
            environment_field: None,
            cause,
        }
    }

    fn for_server(name: &str, cause: WorkspaceDiagnosticCause) -> Self {
        Self {
            server_name: Some(name.to_owned()),
            ..Self::new(cause)
        }
    }
}

pub fn render_workspace_diagnostic(diagnostic: &WorkspaceDiagnostic) -> String {
    let server = encode_terminal_safe(
        diagnostic
            .server_name
            .as_deref()
            .unwrap_or("unknown")
            .as_bytes(),
        MAX_RENDERED_SERVER_NAME_BYTES,
    )
    .text;
    if diagnostic.cause == WorkspaceDiagnosticCause::MissingEnvironmentVariable {
        let variable = encode_terminal_safe(
            diagnostic
                .environment_variable
                .as_deref()
                .unwrap_or("unknown")
                .as_bytes(),
            MAX_RENDERED_ENVIRONMENT_NAME_BYTES,
        )
        .text;
        let field = diagnostic
            .environment_field
            .map_or("value", WorkspaceEnvironmentField::as_str);
        return format!(
            ".mcp.json server '{server}' field {field} requires environment variable '{variable}'; set it or use ${{{variable}:-default}}."
        );
    }
    format!(
        ".mcp.json server '{server}' was skipped: {}.",
        diagnostic.cause.as_str()
    )
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceParseResult {
    pub configs: Vec<McpServerConfig>,
    pub diagnostics: Vec<WorkspaceDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileParseResult {
    pub configs: Vec<McpServerConfig>,
    pub diagnostic: Option<ProfileConfigWarning>,
    pub mutation_allowed: bool,
}

impl Default for ProfileParseResult {
    fn default() -> Self {
        Self {
            configs: Vec::new(),
            diagnostic: None,
            mutation_allowed: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectMcpChoices {
    pub enable_all: bool,
    pub approved: Vec<String>,
    pub rejected: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectMcpAction {
    Approve(String),
    Reject(String),
    ApproveAll,
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMcpTransition {
    pub choices: ProjectMcpChoices,
    pub authority_reduced: bool,
}

impl ProjectMcpChoices {
    pub fn parse(
        workspace: Option<&Map<String, Value>>,
        diagnostics: &mut Vec<WorkspaceDiagnostic>,
    ) -> Result<Self, InvalidProjectMcpChoices> {
        let Some(object) = workspace else {
            return Ok(Self::default());
        };
        let approved = match object.get(ENABLED_SERVERS_KEY) {
            Some(field) => parse_unique_string_array(field)?,
            None => Vec::new(),
        };
        let rejected = match object.get(DISABLED_SERVERS_KEY) {
            Some(field) => parse_unique_string_array(field)?,
            None => Vec::new(),
        };
        let enable_all = match object.get(ENABLE_ALL_KEY) {
            Some(Value::Bool(flag)) => *flag,
            Some(_) => return Err(InvalidProjectMcpChoices),
            None => false,
        };
        let mut normalized = Vec::with_capacity(approved.len());
        for name in approved {
            if rejected.contains(&name) {
                diagnostics.push(WorkspaceDiagnostic::for_server(
                    &name,
                    WorkspaceDiagnosticCause::ApprovedRejectedOverlap,
                ));
            } else {
                normalized.push(name);
            }
        }
        Ok(Self {
            enable_all,
            approved: normalized,
            rejected,
        })
    }

    pub fn admission(&self, name: &str) -> WorkspaceAdmission {
        if self.rejected.iter().any(|value| value == name) {
            WorkspaceAdmission::Rejected
        } else if self.enable_all || self.approved.iter().any(|value| value == name) {
            WorkspaceAdmission::Approved
        } else {
            WorkspaceAdmission::Pending
        }
    }

    pub fn apply(&self, action: &ProjectMcpAction) -> ProjectMcpTransition {
        let mut approved = Vec::new();
        let mut rejected = Vec::new();
        match action {
            ProjectMcpAction::Approve(name) => {
                append_names_except(&mut approved, &self.approved, None);
                append_unique(&mut approved, name);
                append_names_except(&mut rejected, &self.rejected, Some(name));
            }
            ProjectMcpAction::Reject(name) => {
                append_names_except(&mut approved, &self.approved, Some(name));
                append_names_except(&mut rejected, &self.rejected, None);
                append_unique(&mut rejected, name);
            }
            ProjectMcpAction::ApproveAll => {
                append_names_except(&mut approved, &self.approved, None);
                append_names_except(&mut rejected, &self.rejected, None);
            }
            ProjectMcpAction::Reset => {}
        }
        let authority_reduced = match action {
            ProjectMcpAction::Approve(_) | ProjectMcpAction::ApproveAll => false,
            ProjectMcpAction::Reject(name) => {
                self.enable_all || self.approved.iter().any(|value| value == name)
            }
            ProjectMcpAction::Reset => self.enable_all || !self.approved.is_empty(),
        };
        let enable_all = match action {
            ProjectMcpAction::ApproveAll => true,
            ProjectMcpAction::Reset => false,
            _ => self.enable_all,
        };
        ProjectMcpTransition {
            choices: Self {
                enable_all,
                approved,
                rejected,
            },
            authority_reduced,
        }
    }
}

fn append_names_except(output: &mut Vec<String>, input: &[String], excluded: Option<&str>) {
    for name in input {
        if excluded != Some(name.as_str()) {
            append_unique(output, name);
        }
    }
}

fn append_unique(output: &mut Vec<String>, name: &str) {
    if !output.iter().any(|value| value == name) {
        output.push(name.to_owned());
    }
}

fn parse_unique_string_array(value: &Value) -> Result<Vec<String>, InvalidProjectMcpChoices> {
    let names = parse_string_array(value).ok_or(InvalidProjectMcpChoices)?;
    let mut unique = Vec::new();
    for name in names {
        if name.is_empty() {
            return Err(InvalidProjectMcpChoices);
        }
        append_unique(&mut unique, &name);
    }
    Ok(unique)
}

#[derive(Debug, Clone, Copy)]
struct ParsePolicy {
    source: ConfigSource,
    scope: ConfigScope,
    force_optional: bool,
    allow_stored_credentials: bool,
    allow_authorization_header: bool,
    workspace_admission: Option<WorkspaceAdmission>,
}

enum SuspiciousKeyScan<'a> {
    Clear,
    Found {
        key: &'a str,
        additional_matches: usize,
    },
    Indeterminate,
}

#[derive(PartialEq, Eq)]
enum ServerMapShape {
    Clear,
    Match,
    Indeterminate,
}

pub fn parse_profile_document(json_text: &[u8]) -> Result<ProfileParseResult, McpConfigError> {
    let root: Value =
        serde_json::from_slice(json_text).map_err(|_| McpConfigError::McpConfigInvalidJson)?;
    let object = root
        .as_object()
        .ok_or(McpConfigError::McpConfigRootMustBeObject)?;
    let canonical = object.get("mcp");
    let alias = object.get("mcpServers");
    let configs = match canonical.or(alias) {
        Some(servers) => parse_profile_server_map(servers)?,
        None => Vec::new(),
    };
    let mut result = ProfileParseResult {
        configs,
        ..ProfileParseResult::default()
    };
    match scan_suspicious_profile_keys(object) {
        SuspiciousKeyScan::Clear => {
            if canonical.is_some() && alias.is_some_and(profile_alias_is_non_empty) {
                result.diagnostic = Some(ProfileConfigWarning::new(
                    ProfileConfigWarningCause::IgnoredMcpServersAlias,
                    Some("mcpServers"),
                    0,
                ));
            }
        }
        SuspiciousKeyScan::Found {
            key,
            additional_matches,
        } => {
            result.diagnostic = Some(ProfileConfigWarning::new(
                ProfileConfigWarningCause::SuspiciousServerKey,
                Some(key),
                additional_matches,
            ));
            result.mutation_allowed = false;
        }
        SuspiciousKeyScan::Indeterminate => {
            result.diagnostic = Some(ProfileConfigWarning::new(
                ProfileConfigWarningCause::SuspiciousKeyScanIndeterminate,
                None,
                0,
            ));
            result.mutation_allowed = false;
        }
    }
    Ok(result)
}

fn parse_profile_server_map(servers: &Value) -> Result<Vec<McpServerConfig>, McpConfigError> {
    let servers = servers
        .as_object()
        .ok_or(McpConfigError::McpConfigServersMustBeObject)?;
    let policy = ParsePolicy {
        source: ConfigSource::Profile,
        scope: ConfigScope::Profile,
        force_optional: false,
        allow_stored_credentials: true,
        allow_authorization_header: false,
        workspace_admission: None,
    };
    servers
        .iter()
        .map(|(name, value)| parse_server_entry(name, value, policy))
        .collect()
}

fn profile_alias_is_non_empty(value: &Value) -> bool {
    match value {
        Value::Object(object) => !object.is_empty(),
        Value::Array(array) => !array.is_empty(),
        Value::String(text) => !text.is_empty(),
        Value::Null => false,
        Value::Bool(_) | Value::Number(_) => true,
    }
}

fn scan_suspicious_profile_keys(object: &Map<String, Value>) -> SuspiciousKeyScan<'_> {
    if object.len() > MAX_PROFILE_ROOT_SCAN_ENTRIES {
        return SuspiciousKeyScan::Indeterminate;
    }
    let mut first = None;
    let mut additional_matches = 0;
    for (key, value) in object {
        if key == "mcp" || key == "mcpServers" {
            continue;
        }
        if key.len() > MAX_PROFILE_CONFIG_WARNING_KEY_BYTES {
            if server_map_shape(value) != ServerMapShape::Clear {
                return SuspiciousKeyScan::Indeterminate;
            }
            continue;
        }
        if !is_suspicious_normalized_key(key) {
            continue;
        }
        match server_map_shape(value) {
            ServerMapShape::Clear => {}
            ServerMapShape::Match => {
                if first.is_none() {
                    first = Some(key.as_str());
                } else {
                    additional_matches += 1;
                }
            }
            ServerMapShape::Indeterminate => return SuspiciousKeyScan::Indeterminate,
        }
    }
    match first {
        Some(key) => SuspiciousKeyScan::Found {
            key,
            additional_matches,
        },
        None => SuspiciousKeyScan::Clear,
    }
}

fn server_map_shape(value: &Value) -> ServerMapShape {
    let Some(object) = value.as_object() else {
        return ServerMapShape::Clear;
    };
    for (inspected, entry) in object.values().enumerate() {
        if inspected == MAX_PROFILE_CHILD_SCAN_ENTRIES {
            return ServerMapShape::Indeterminate;
        }
        if server_entry_shape(entry) {
            return ServerMapShape::Match;
        }
    }
    ServerMapShape::Clear
}

fn server_entry_shape(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if matches!(
        object.get("command"),
        Some(Value::String(_) | Value::Array(_))
    ) {
        return true;
    }
    if matches!(object.get("url"), Some(Value::String(_))) {
        return true;
    }
    match object.get("type") {
        Some(Value::String(kind)) => matches!(kind.as_str(), "local" | "stdio" | "http" | "sse"),
        _ => false,
    }
}

fn is_suspicious_normalized_key(raw: &str) -> bool {
    let normalized: String = raw
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(|byte| char::from(byte.to_ascii_lowercase()))
        .collect();
    matches!(
        normalized.as_str(),
        "mcpserver"
            | "mcpservers"
            | "modelcontextprotocol"
            | "modelcontextprotocolserver"
            | "modelcontextprotocolservers"
            | "servers"
    )
}

pub fn parse_workspace_document(
    json_text: &[u8],
    choices: &ProjectMcpChoices,
) -> WorkspaceParseResult {
    let mut result = WorkspaceParseResult::default();
    let Ok(root) = serde_json::from_slice::<Value>(json_text) else {
        result.diagnostics.push(WorkspaceDiagnostic::new(
            WorkspaceDiagnosticCause::InvalidJson,
        ));
        return result;
    };
    let Some(object) = root.as_object() else {
        result.diagnostics.push(WorkspaceDiagnostic::new(
            WorkspaceDiagnosticCause::RootMustBeObject,
        ));
        return result;
    };
    let Some(servers) = object.get("mcpServers") else {
        return result;
    };
    let Some(servers) = servers.as_object() else {
        result.diagnostics.push(WorkspaceDiagnostic::new(
            WorkspaceDiagnosticCause::ServersMustBeObject,
        ));
        return result;
    };
    for (name, value) in servers {
        let policy = ParsePolicy {
            source: ConfigSource::Workspace,
            scope: ConfigScope::Workspace,
            force_optional: true,
            allow_stored_credentials: false,
            allow_authorization_header: true,
            workspace_admission: Some(choices.admission(name)),
        };
        match parse_server_entry(name, value, policy) {
            Ok(config) => result.configs.push(config),
            Err(_) => result.diagnostics.push(WorkspaceDiagnostic::for_server(
                name,
                WorkspaceDiagnosticCause::InvalidEntry,
            )),
        }
    }
    result
}

pub fn expand_approved_workspace_configs(
    result: &mut WorkspaceParseResult,
    environment: &dyn Fn(&str) -> Option<String>,
) {
    let mut budget = ExpansionBudget {
        remaining: MAX_EXPANDED_WORKSPACE_TOTAL_BYTES,
    };
    let configs = std::mem::take(&mut result.configs);
    for mut config in configs {
        if config.workspace_admission != Some(WorkspaceAdmission::Approved) {
            result.configs.push(config);
            continue;
        }
        match expand_workspace_config(&mut config, environment, &mut budget) {
            None => result.configs.push(config),
            Some(failure) => result
                .diagnostics
                .push(failure.into_diagnostic(&config.name)),
        }
    }
}

struct ExpansionBudget {
    remaining: usize,
}

impl ExpansionBudget {
    fn consume(&mut self, byte_count: usize) -> bool {
        if byte_count > self.remaining {
            self.remaining = 0;
            return false;
        }
        self.remaining -= byte_count;
        true
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TemplateExpansion {
    Expanded(String),
    Missing(String),
    Invalid,
    LimitExceeded,
}

enum ExpansionFailure {
    Missing {
        field: WorkspaceEnvironmentField,
        variable_name: String,
    },
    Invalid,
    LimitExceeded,
}

impl ExpansionFailure {
    fn into_diagnostic(self, server_name: &str) -> WorkspaceDiagnostic {
        match self {
            Self::Missing {
                field,
                variable_name,
            } => WorkspaceDiagnostic {
                server_name: Some(server_name.to_owned()),
                environment_variable: Some(variable_name),
                environment_field: Some(field),
                cause: WorkspaceDiagnosticCause::MissingEnvironmentVariable,
            },
            Self::Invalid => {
                WorkspaceDiagnostic::for_server(server_name, WorkspaceDiagnosticCause::InvalidEntry)
            }
            Self::LimitExceeded => WorkspaceDiagnostic::for_server(
                server_name,
                WorkspaceDiagnosticCause::EnvironmentExpansionLimitExceeded,
            ),
        }
    }
}

fn expand_workspace_template(
    input: &str,
    environment: &dyn Fn(&str) -> Option<String>,
    budget: &mut ExpansionBudget,
) -> TemplateExpansion {
    let mut output = String::new();
    let mut rest = input;
    while let Some((text, after)) = rest.split_once("${") {
        if !append_expanded(&mut output, text, budget) {
            return TemplateExpansion::LimitExceeded;
        }
        let Some((expression, remainder)) = after.split_once('}') else {
            return TemplateExpansion::Invalid;
        };
        let (variable_name, default_value) = match expression.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (expression, None),
        };
        if !is_valid_env_name(variable_name) {
            return TemplateExpansion::Invalid;
        }
        let replacement = match environment(variable_name) {
            Some(value) => value,
            None => match default_value {
                Some(default) => default.to_owned(),
                None => return TemplateExpansion::Missing(variable_name.to_owned()),
            },
        };
        if !append_expanded(&mut output, &replacement, budget) {
            return TemplateExpansion::LimitExceeded;
        }
        rest = remainder;
    }
    if !append_expanded(&mut output, rest, budget) {
        return TemplateExpansion::LimitExceeded;
    }
    TemplateExpansion::Expanded(output)
}

fn append_expanded(output: &mut String, bytes: &str, budget: &mut ExpansionBudget) -> bool {
    if output.len() > MAX_EXPANDED_WORKSPACE_VALUE_BYTES
        || bytes.len() > MAX_EXPANDED_WORKSPACE_VALUE_BYTES - output.len()
    {
        return false;
    }
    if !budget.consume(bytes.len()) {
        return false;
    }
    output.push_str(bytes);
    true
}

fn expand_workspace_config(
    config: &mut McpServerConfig,
    environment: &dyn Fn(&str) -> Option<String>,
    budget: &mut ExpansionBudget,
) -> Option<ExpansionFailure> {
    let mut targets: Vec<(&mut String, WorkspaceEnvironmentField)> = Vec::new();
    if let Some(command) = config.command.as_mut() {
        targets.push((command, WorkspaceEnvironmentField::Command));
    }
    targets.extend(
        config
            .args
            .iter_mut()
            .map(|argument| (argument, WorkspaceEnvironmentField::Argument)),
    );
    targets.extend(
        config
            .env
            .iter_mut()
            .map(|entry| (&mut entry.value, WorkspaceEnvironmentField::Environment)),
    );
    targets.extend(
        config
            .headers
            .iter_mut()
            .map(|header| (&mut header.value, WorkspaceEnvironmentField::HttpHeader)),
    );
    for (value, field) in targets {
        match expand_workspace_template(value, environment, budget) {
            TemplateExpansion::Expanded(expanded) => *value = expanded,
            TemplateExpansion::Missing(variable_name) => {
                return Some(ExpansionFailure::Missing {
                    field,
                    variable_name,
                });
            }
            TemplateExpansion::Invalid => return Some(ExpansionFailure::Invalid),
            TemplateExpansion::LimitExceeded => return Some(ExpansionFailure::LimitExceeded),
        }
    }
    validate_static_headers(&config.headers)
        .err()
        .map(|_| ExpansionFailure::Invalid)
}

pub fn merge_native(
    profile: Vec<McpServerConfig>,
    workspace: Vec<McpServerConfig>,
) -> Vec<McpServerConfig> {
    let mut merged = profile;
    for candidate in workspace {
        if !merged.iter().any(|config| config.name == candidate.name) {
            merged.push(candidate);
        }
    }
    merged
}

fn parse_server_entry(
    name: &str,
    value: &Value,
    policy: ParsePolicy,
) -> Result<McpServerConfig, McpConfigError> {
    let object = value
        .as_object()
        .ok_or(McpConfigError::McpConfigServerMustBeObject)?;
    if !source_allows_scope(policy.source, policy.scope)
        || !source_allows_workspace_admission(policy.source, policy.workspace_admission)
    {
        return Err(McpConfigError::McpConfigPolicyInvalid);
    }
    let type_string = match object.get("type") {
        Some(Value::String(kind)) => kind.as_str(),
        Some(_) => return Err(McpConfigError::McpConfigInvalidType),
        None => "local",
    };
    let transport = match type_string {
        "sse" => TransportType::Sse,
        "http" => TransportType::Http,
        "local" | "stdio" => TransportType::Stdio,
        _ => return Err(McpConfigError::McpConfigInvalidType),
    };
    let enabled =
        optional_bool(object, "enabled", true).ok_or(McpConfigError::McpConfigInvalidEnabled)?;
    let configured_required =
        optional_bool(object, "required", false).ok_or(McpConfigError::McpConfigInvalidRequired)?;
    let mut config = McpServerConfig::new(name, transport);
    config.source = policy.source;
    config.scope = policy.scope;
    config.required = configured_required && !policy.force_optional;
    config.enabled = enabled;
    config.workspace_admission = policy.workspace_admission;
    if transport == TransportType::Stdio {
        parse_stdio_fields(object, &mut config)?;
    } else {
        parse_remote_fields(object, policy, &mut config)?;
    }
    Ok(config)
}

fn parse_remote_fields(
    object: &Map<String, Value>,
    policy: ParsePolicy,
    config: &mut McpServerConfig,
) -> Result<(), McpConfigError> {
    let url = match object.get("url") {
        Some(Value::String(url)) => url,
        Some(_) => return Err(McpConfigError::McpConfigInvalidUrl),
        None => return Err(McpConfigError::McpConfigMissingUrl),
    };
    validate_endpoint(url).map_err(|_| McpConfigError::McpConfigInvalidUrl)?;
    (config.startup_timeout_ms, config.operation_timeout_ms) = parse_timeouts(object)?;
    config.env = parse_selected_environment(object)?;
    config.headers = parse_remote_headers(object, policy.allow_authorization_header)
        .ok_or(McpConfigError::McpConfigInvalidHeaders)?;
    config.header_env =
        parse_remote_header_env(object).ok_or(McpConfigError::McpConfigInvalidHeaders)?;
    config.bearer_token_env = parse_optional_string(object, "bearer_token_env")
        .map_err(|_| McpConfigError::McpConfigInvalidOAuth)?;
    if config
        .bearer_token_env
        .as_deref()
        .is_some_and(|name| !is_valid_env_name(name))
    {
        return Err(McpConfigError::McpConfigInvalidBearerEnvironment);
    }
    config.auth = parse_profile_auth(object).map_err(|_| McpConfigError::McpConfigInvalidOAuth)?;
    config.url = Some(url.clone());
    config.allow_stored_credentials = policy.allow_stored_credentials;
    Ok(())
}

fn parse_stdio_fields(
    object: &Map<String, Value>,
    config: &mut McpServerConfig,
) -> Result<(), McpConfigError> {
    (config.startup_timeout_ms, config.operation_timeout_ms) = parse_timeouts(object)?;
    let restart_limit = parse_unsigned_policy(
        object,
        "restart_limit",
        u64::from(DEFAULT_RESTART_LIMIT),
        0,
        u64::from(u8::MAX),
    )
    .ok_or(McpConfigError::McpConfigInvalidRestartLimit)?;
    config.restart_limit =
        u8::try_from(restart_limit).map_err(|_| McpConfigError::McpConfigInvalidRestartLimit)?;
    let (command, args) =
        parse_command_spec(object).ok_or(McpConfigError::McpConfigInvalidCommand)?;
    config.command = Some(command);
    config.args = args;
    config.env = parse_selected_environment(object)?;
    Ok(())
}

fn parse_timeouts(object: &Map<String, Value>) -> Result<(u32, u32), McpConfigError> {
    let startup = parse_unsigned_policy(
        object,
        "startup_timeout_ms",
        u64::from(DEFAULT_STARTUP_TIMEOUT_MS),
        1,
        u64::from(u32::MAX),
    )
    .and_then(|value| u32::try_from(value).ok())
    .ok_or(McpConfigError::McpConfigInvalidStartupTimeout)?;
    let operation = parse_unsigned_policy(
        object,
        "operation_timeout_ms",
        u64::from(DEFAULT_OPERATION_TIMEOUT_MS),
        1,
        u64::from(u32::MAX),
    )
    .and_then(|value| u32::try_from(value).ok())
    .ok_or(McpConfigError::McpConfigInvalidOperationTimeout)?;
    Ok((startup, operation))
}

fn parse_unsigned_policy(
    object: &Map<String, Value>,
    key: &str,
    default_value: u64,
    min_value: u64,
    max_value: u64,
) -> Option<u64> {
    let Some(value) = object.get(key) else {
        return Some(default_value);
    };
    let parsed = value.as_u64()?;
    (min_value..=max_value).contains(&parsed).then_some(parsed)
}

fn optional_bool(object: &Map<String, Value>, key: &str, default_value: bool) -> Option<bool> {
    match object.get(key) {
        Some(Value::Bool(flag)) => Some(*flag),
        Some(_) => None,
        None => Some(default_value),
    }
}

fn parse_remote_headers(
    object: &Map<String, Value>,
    allow_authorization: bool,
) -> Option<Vec<HttpHeader>> {
    let Some(value) = object.get("headers") else {
        return Some(Vec::new());
    };
    let mut headers = Vec::new();
    for (name, value) in value.as_object()? {
        let value = value.as_str()?;
        if !allow_authorization && name.eq_ignore_ascii_case("authorization") {
            return None;
        }
        headers.push(HttpHeader {
            name: name.clone(),
            value: value.to_owned(),
        });
    }
    validate_static_headers(&headers).ok()?;
    Some(headers)
}

fn parse_remote_header_env(object: &Map<String, Value>) -> Option<Vec<HttpHeaderEnv>> {
    let Some(value) = object.get("header_env") else {
        return Some(Vec::new());
    };
    let mut refs: Vec<HttpHeaderEnv> = Vec::new();
    for (name, value) in value.as_object()? {
        let env = value.as_str()?;
        if !is_valid_env_name(env) || name.eq_ignore_ascii_case("authorization") {
            return None;
        }
        if refs
            .iter()
            .any(|previous| previous.name.eq_ignore_ascii_case(name))
        {
            return None;
        }
        refs.push(HttpHeaderEnv {
            name: name.clone(),
            env: env.to_owned(),
        });
    }
    let validation: Vec<HttpHeader> = refs
        .iter()
        .map(|reference| HttpHeader {
            name: reference.name.clone(),
            value: "value".to_owned(),
        })
        .collect();
    validate_static_headers(&validation).ok()?;
    Some(refs)
}

fn parse_profile_auth(object: &Map<String, Value>) -> Result<Option<McpAuthConfig>, InvalidField> {
    let Some(value) = object.get("oauth") else {
        return Ok(None);
    };
    let auth_object = value.as_object().ok_or(InvalidField)?;
    let mut auth = McpAuthConfig {
        resource: parse_optional_string(auth_object, "resource")?,
        issuer: parse_optional_string(auth_object, "issuer")?,
        client_id: parse_optional_string(auth_object, "client_id")?,
        client_secret_env: parse_optional_string(auth_object, "client_secret_env")?,
        client_metadata_url: parse_optional_string(auth_object, "client_metadata_url")?,
        ..McpAuthConfig::default()
    };
    if let Some(scopes) = auth_object.get("scopes") {
        auth.scopes = parse_string_array(scopes).ok_or(InvalidField)?;
        auth.scopes_configured = true;
    }
    if let Some(port) = auth_object.get("callback_port") {
        let port = port
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port >= 1)
            .ok_or(InvalidField)?;
        auth.callback_port = Some(port);
    }
    let invalid_secret = auth
        .client_secret_env
        .as_deref()
        .is_some_and(|secret_env| !is_valid_env_name(secret_env) || auth.client_id.is_none());
    let invalid_resource = auth
        .resource
        .as_deref()
        .is_some_and(|resource| !is_canonical_resource(resource));
    let invalid_metadata_url = auth
        .client_metadata_url
        .as_deref()
        .is_some_and(|url| !is_valid_client_metadata_url(url));
    if invalid_secret || invalid_resource || invalid_metadata_url {
        return Err(InvalidField);
    }
    Ok(Some(auth))
}

fn is_canonical_resource(endpoint: &str) -> bool {
    let Some(uri) = Uri::parse(endpoint) else {
        return false;
    };
    if uri.has_userinfo || uri.has_fragment || uri.host.is_none_or(str::is_empty) {
        return false;
    }
    uri.scheme.eq_ignore_ascii_case("https")
        || (uri.scheme.eq_ignore_ascii_case("http") && uri.port.is_some() && uri.is_loopback_host())
}

fn is_valid_client_metadata_url(url: &str) -> bool {
    let Some(uri) = Uri::parse(url) else {
        return false;
    };
    uri.scheme.eq_ignore_ascii_case("https")
        && uri.host.is_some()
        && !uri.has_userinfo
        && !uri.has_fragment
        && !uri.path.trim_matches('/').is_empty()
}

struct InvalidField;

fn parse_optional_string(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, InvalidField> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.trim_matches([' ', '\t', '\r', '\n']).is_empty() => {
            Ok(Some(text.clone()))
        }
        Some(_) => Err(InvalidField),
    }
}

pub(crate) fn is_valid_env_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn parse_selected_environment(object: &Map<String, Value>) -> Result<Vec<EnvVar>, McpConfigError> {
    let Some(value) = object.get("environment").or_else(|| object.get("env")) else {
        return Ok(Vec::new());
    };
    let entries = value
        .as_object()
        .ok_or(McpConfigError::McpConfigInvalidEnvironment)?;
    entries
        .iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| EnvVar {
                    key: key.clone(),
                    value: value.to_owned(),
                })
                .ok_or(McpConfigError::McpConfigInvalidEnvironment)
        })
        .collect()
}

fn parse_command_spec(object: &Map<String, Value>) -> Option<(String, Vec<String>)> {
    match object.get("command")? {
        Value::String(command) => {
            let args = match object.get("args") {
                Some(args) => parse_string_array(args)?,
                None => Vec::new(),
            };
            Some((command.clone(), args))
        }
        Value::Array(items) => {
            let (first, rest) = items.split_first()?;
            let command = first.as_str()?.to_owned();
            let args = rest
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()?;
            Some((command, args))
        }
        _ => None,
    }
}

fn parse_string_array(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn environment(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn approved(names: &[&str]) -> ProjectMcpChoices {
        ProjectMcpChoices {
            approved: names.iter().map(|name| (*name).to_owned()).collect(),
            ..ProjectMcpChoices::default()
        }
    }

    fn workspace_with_environment(
        json: &str,
        choices: &ProjectMcpChoices,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> WorkspaceParseResult {
        let mut result = parse_workspace_document(json.as_bytes(), choices);
        expand_approved_workspace_configs(&mut result, lookup);
        result
    }

    fn fresh_budget() -> ExpansionBudget {
        ExpansionBudget {
            remaining: MAX_EXPANDED_WORKSPACE_TOTAL_BYTES,
        }
    }

    #[test]
    fn server_reuse_compares_owned_configuration_values_and_all_authority_fields() {
        let first = McpServerConfig::stdio("one", "node", vec!["server.js".to_owned()]);
        let mut second = first.clone();
        assert_eq!(first, second);
        second.args = vec!["other.js".to_owned()];
        assert_ne!(first, second);
        second = first.clone();
        second.allow_stored_credentials = true;
        assert_ne!(first, second);
        let mut workspace = first.clone();
        workspace.source = ConfigSource::Workspace;
        workspace.scope = ConfigScope::Workspace;
        workspace.workspace_admission = Some(WorkspaceAdmission::Approved);
        let mut rejected = workspace.clone();
        rejected.workspace_admission = Some(WorkspaceAdmission::Rejected);
        assert_ne!(workspace, rejected);
    }

    #[test]
    fn profile_document_accepts_mcp_servers_alias_and_preserves_strict_entry_parsing() {
        let result = parse_profile_document(
            br#"{"mcpServers":{"local":{"command":"node","args":["server.js"]}}}"#,
        )
        .unwrap();
        assert_eq!(result.configs.len(), 1);
        assert_eq!(result.configs[0].name, "local");
        assert_eq!(result.configs[0].command.as_deref(), Some("node"));
        assert_eq!(result.diagnostic, None);
        assert!(result.mutation_allowed);
    }

    #[test]
    fn profile_document_keeps_canonical_mcp_and_reports_ignored_nonempty_alias() {
        let result = parse_profile_document(
            br#"{"mcp":{"canonical":{"command":"one"}},"mcpServers":{"ignored":{"command":"two"}}}"#,
        )
        .unwrap();
        assert_eq!(result.configs.len(), 1);
        assert_eq!(result.configs[0].name, "canonical");
        assert_eq!(
            result.diagnostic.map(|warning| warning.cause),
            Some(ProfileConfigWarningCause::IgnoredMcpServersAlias)
        );
        assert!(result.mutation_allowed);
    }

    #[test]
    fn profile_document_blocks_suspicious_sibling_keys_beside_canonical_mcp() {
        let result = parse_profile_document(
            br#"{"mcp":{"canonical":{"command":"one"}},"MCP-Servers":{"shadow":{"command":"two"}},"metadata":{"owner":"team"}}"#,
        )
        .unwrap();
        assert_eq!(result.configs.len(), 1);
        assert_eq!(result.configs[0].name, "canonical");
        let warning = result.diagnostic.unwrap();
        assert_eq!(
            warning.cause,
            ProfileConfigWarningCause::SuspiciousServerKey
        );
        assert_eq!(warning.key.as_deref(), Some("MCP-Servers"));
        assert!(!result.mutation_allowed);
    }

    #[test]
    fn profile_document_reports_the_first_exact_server_like_unsupported_key() {
        let result = parse_profile_document(
            br#"{"metadata":{"entry":{"command":"ignored"}},"MCP-Servers":{"one":{"command":"node"}},"servers":{"two":{"url":"https://example.test/mcp"}}}"#,
        )
        .unwrap();
        assert!(result.configs.is_empty());
        let warning = result.diagnostic.unwrap();
        assert_eq!(
            warning.cause,
            ProfileConfigWarningCause::SuspiciousServerKey
        );
        assert_eq!(warning.key.as_deref(), Some("MCP-Servers"));
        assert_eq!(warning.additional_matches, 1);
        assert!(!result.mutation_allowed);
    }

    #[test]
    fn profile_document_bounds_suspicious_key_scans() {
        let members: Vec<String> = (0..65)
            .map(|index| format!("\"metadata{index}\":{{}}"))
            .collect();
        let json = format!("{{{}}}", members.join(","));
        let result = parse_profile_document(json.as_bytes()).unwrap();
        assert_eq!(
            result.diagnostic.map(|warning| warning.cause),
            Some(ProfileConfigWarningCause::SuspiciousKeyScanIndeterminate)
        );
        assert!(!result.mutation_allowed);
    }

    #[test]
    fn profile_parser_preserves_validation_precedence_for_multiply_invalid_entries() {
        let cases: [(&str, McpConfigError); 5] = [
            (
                r#"{"mcp":{"bad":{"type":"http","startup_timeout_ms":0,"env":3}}}"#,
                McpConfigError::McpConfigMissingUrl,
            ),
            (
                r#"{"mcp":{"bad":{"type":"http","url":"https://example.test/mcp","startup_timeout_ms":0,"env":3}}}"#,
                McpConfigError::McpConfigInvalidStartupTimeout,
            ),
            (
                r#"{"mcp":{"bad":{"startup_timeout_ms":0,"restart_limit":999,"command":3,"env":3}}}"#,
                McpConfigError::McpConfigInvalidStartupTimeout,
            ),
            (
                r#"{"mcp":{"bad":{"restart_limit":999,"command":3,"env":3}}}"#,
                McpConfigError::McpConfigInvalidRestartLimit,
            ),
            (
                r#"{"mcp":{"bad":{"command":3,"env":3}}}"#,
                McpConfigError::McpConfigInvalidCommand,
            ),
        ];
        for (json, expected) in cases {
            assert_eq!(
                parse_profile_document(json.as_bytes()),
                Err(expected),
                "{json}"
            );
        }
    }

    #[test]
    fn workspace_parsing_stamps_optional_credential_isolated_configs() {
        let result = parse_workspace_document(
            br#"{"mcpServers":{"local":{"command":"node","args":["server.js"],"required":true},"remote":{"type":"http","url":"https://example.test/mcp"}}}"#,
            &ProjectMcpChoices::default(),
        );
        assert_eq!(result.configs.len(), 2);
        for config in &result.configs {
            assert_eq!(config.source, ConfigSource::Workspace);
            assert_eq!(config.scope, ConfigScope::Workspace);
            assert!(!config.required);
            assert!(!config.allow_stored_credentials);
            assert_eq!(
                config.workspace_admission,
                Some(WorkspaceAdmission::Pending)
            );
        }
    }

    #[test]
    fn workspace_template_expansion_is_explicit_bounded_and_deterministic() {
        let long_name = format!("A{}", "B".repeat(128));
        let lookup = environment(&[
            ("SET", "value"),
            ("EMPTY", ""),
            (&long_name, "long-name-value"),
        ]);
        let cases = [
            ("plain", "plain"),
            ("before-${SET}-after", "before-value-after"),
            ("${MISSING:-fallback}", "fallback"),
            ("${EMPTY:-fallback}", ""),
            ("${SET}/${SET}", "value/value"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                expand_workspace_template(input, &lookup, &mut fresh_budget()),
                TemplateExpansion::Expanded(expected.to_owned())
            );
        }
        assert_eq!(
            expand_workspace_template("Bearer ${REQUIRED}", &lookup, &mut fresh_budget()),
            TemplateExpansion::Missing("REQUIRED".to_owned())
        );
        assert_eq!(
            expand_workspace_template("${NOT-CLOSED", &lookup, &mut fresh_budget()),
            TemplateExpansion::Invalid
        );
        assert_eq!(
            expand_workspace_template(&format!("${{{long_name}}}"), &lookup, &mut fresh_budget()),
            TemplateExpansion::Expanded("long-name-value".to_owned())
        );
    }

    #[test]
    fn workspace_environment_diagnostics_isolate_entries_and_profile_values_stay_literal() {
        let lookup = environment(&[("COMMAND", "node")]);
        let workspace = workspace_with_environment(
            r#"{"mcpServers":{"good":{"command":"${COMMAND}"},"bad":{"command":"${REQUIRED}"}}}"#,
            &approved(&["good", "bad"]),
            &lookup,
        );
        assert_eq!(workspace.configs.len(), 1);
        assert_eq!(workspace.configs[0].command.as_deref(), Some("node"));
        assert_eq!(
            workspace.diagnostics,
            vec![WorkspaceDiagnostic {
                server_name: Some("bad".to_owned()),
                environment_variable: Some("REQUIRED".to_owned()),
                environment_field: Some(WorkspaceEnvironmentField::Command),
                cause: WorkspaceDiagnosticCause::MissingEnvironmentVariable,
            }]
        );
        let profile =
            parse_profile_document(br#"{"mcp":{"literal":{"command":"${COMMAND}"}}}"#).unwrap();
        assert_eq!(profile.configs[0].command.as_deref(), Some("${COMMAND}"));
    }

    #[test]
    fn workspace_environment_expansion_requires_approved_admission() {
        let lookup = environment(&[("COMMAND", "node"), ("VALUE", "expanded")]);
        let workspace = workspace_with_environment(
            r#"{"mcpServers":{"local":{"command":"${COMMAND}","args":["${VALUE}"],"env":{"TOKEN":"${VALUE}"}},"remote":{"type":"http","url":"https://example.test/mcp","headers":{"Authorization":"Bearer ${VALUE}"}}}}"#,
            &ProjectMcpChoices::default(),
            &lookup,
        );
        assert_eq!(workspace.configs.len(), 2);
        assert_eq!(workspace.configs[0].command.as_deref(), Some("${COMMAND}"));
        assert_eq!(workspace.configs[0].args, vec!["${VALUE}".to_owned()]);
        assert_eq!(workspace.configs[0].env[0].value, "${VALUE}");
        assert_eq!(workspace.configs[1].headers[0].value, "Bearer ${VALUE}");
        assert!(workspace.diagnostics.is_empty());
    }

    #[test]
    fn workspace_environment_expansion_enforces_one_aggregate_file_budget() {
        let large = "x".repeat(600 * 1024);
        let lookup = environment(&[("LARGE", &large)]);
        let workspace = workspace_with_environment(
            r#"{"mcpServers":{"first":{"command":"${LARGE}"},"second":{"command":"${LARGE}"}}}"#,
            &approved(&["first", "second"]),
            &lookup,
        );
        assert_eq!(workspace.configs.len(), 1);
        assert_eq!(workspace.configs[0].name, "first");
        assert_eq!(
            workspace.diagnostics,
            vec![WorkspaceDiagnostic::for_server(
                "second",
                WorkspaceDiagnosticCause::EnvironmentExpansionLimitExceeded
            )]
        );
    }

    #[test]
    fn workspace_environment_expansion_never_refunds_rejected_entry_work() {
        let large = "x".repeat(600 * 1024);
        let lookup = environment(&[("LARGE", &large)]);
        let workspace = workspace_with_environment(
            r#"{"mcpServers":{"rejected_late":{"command":"${LARGE}","args":["${MISSING}"]},"later":{"command":"${LARGE}"}}}"#,
            &approved(&["rejected_late", "later"]),
            &lookup,
        );
        assert!(workspace.configs.is_empty());
        let causes: Vec<_> = workspace
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.cause)
            .collect();
        assert_eq!(
            causes,
            vec![
                WorkspaceDiagnosticCause::MissingEnvironmentVariable,
                WorkspaceDiagnosticCause::EnvironmentExpansionLimitExceeded,
            ]
        );
    }

    #[test]
    fn workspace_environment_expansion_rejects_exhausted_budget_before_allocation() {
        let lookup = environment(&[("TOKEN", "secret")]);
        let mut budget = ExpansionBudget { remaining: 0 };
        assert_eq!(
            expand_workspace_template("${TOKEN}", &lookup, &mut budget),
            TemplateExpansion::LimitExceeded
        );
    }

    #[test]
    fn workspace_authorization_header_expansion_does_not_broaden_profile_headers() {
        let lookup = environment(&[("TOKEN", "secret")]);
        let json = r#"{"mcpServers":{"remote":{"type":"http","url":"https://example.test/mcp","headers":{"Authorization":"Bearer ${TOKEN}"}}}}"#;
        let workspace = workspace_with_environment(json, &approved(&["remote"]), &lookup);
        assert_eq!(workspace.configs.len(), 1);
        assert_eq!(workspace.configs[0].headers[0].value, "Bearer secret");
        assert_eq!(
            parse_profile_document(
                br#"{"mcp":{"remote":{"type":"http","url":"https://example.test/mcp","headers":{"Authorization":"Bearer ${TOKEN}"}}}}"#
            ),
            Err(McpConfigError::McpConfigInvalidHeaders)
        );
    }

    #[test]
    fn workspace_parsing_isolates_invalid_entries() {
        let result = parse_workspace_document(
            br#"{"mcpServers":{"good":{"command":["node","server.js"]},"bad":{"command":3}}}"#,
            &ProjectMcpChoices::default(),
        );
        assert_eq!(result.configs.len(), 1);
        assert_eq!(result.configs[0].name, "good");
        assert_eq!(
            result.diagnostics,
            vec![WorkspaceDiagnostic::for_server(
                "bad",
                WorkspaceDiagnosticCause::InvalidEntry
            )]
        );
    }

    #[test]
    fn workspace_document_reports_root_level_failures() {
        let choices = ProjectMcpChoices::default();
        let causes = |json: &[u8]| -> Vec<WorkspaceDiagnosticCause> {
            parse_workspace_document(json, &choices)
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.cause)
                .collect()
        };
        assert_eq!(causes(b"{"), vec![WorkspaceDiagnosticCause::InvalidJson]);
        assert_eq!(
            causes(b"[]"),
            vec![WorkspaceDiagnosticCause::RootMustBeObject]
        );
        assert_eq!(
            causes(br#"{"mcpServers":[]}"#),
            vec![WorkspaceDiagnosticCause::ServersMustBeObject]
        );
        assert!(causes(br#"{"other":1}"#).is_empty());
    }

    #[test]
    fn choice_transitions_are_normalized_and_reject_precedence_is_stable() {
        let current = ProjectMcpChoices {
            enable_all: true,
            approved: vec!["alpha".to_owned(), "beta".to_owned()],
            rejected: vec!["gamma".to_owned()],
        };
        let rejected = current.apply(&ProjectMcpAction::Reject("alpha".to_owned()));
        assert!(rejected.authority_reduced);
        assert_eq!(
            rejected.choices.admission("alpha"),
            WorkspaceAdmission::Rejected
        );
        assert_eq!(
            rejected.choices.admission("beta"),
            WorkspaceAdmission::Approved
        );
        let approved = rejected
            .choices
            .apply(&ProjectMcpAction::Approve("alpha".to_owned()));
        assert!(!approved.authority_reduced);
        assert_eq!(
            approved.choices.admission("alpha"),
            WorkspaceAdmission::Approved
        );
    }

    #[test]
    fn choice_parsing_drops_overlaps_and_rejects_malformed_lists() {
        let mut diagnostics = Vec::new();
        let value = serde_json::json!({
            "enabledMcpjsonServers": ["alpha", "overlap", "alpha"],
            "disabledMcpjsonServers": ["overlap", "beta"],
            "enableAllProjectMcpServers": true,
        });
        let choices = ProjectMcpChoices::parse(value.as_object(), &mut diagnostics).unwrap();
        assert_eq!(choices.approved, vec!["alpha".to_owned()]);
        assert_eq!(
            choices.rejected,
            vec!["overlap".to_owned(), "beta".to_owned()]
        );
        assert!(choices.enable_all);
        assert_eq!(
            diagnostics,
            vec![WorkspaceDiagnostic::for_server(
                "overlap",
                WorkspaceDiagnosticCause::ApprovedRejectedOverlap
            )]
        );
        let empty_name = serde_json::json!({"enabledMcpjsonServers": [""]});
        assert_eq!(
            ProjectMcpChoices::parse(empty_name.as_object(), &mut Vec::new()),
            Err(InvalidProjectMcpChoices)
        );
        let bad_flag = serde_json::json!({"enableAllProjectMcpServers": "yes"});
        assert_eq!(
            ProjectMcpChoices::parse(bad_flag.as_object(), &mut Vec::new()),
            Err(InvalidProjectMcpChoices)
        );
    }

    #[test]
    fn reset_reduces_authority_only_when_something_was_approved() {
        let empty = ProjectMcpChoices::default();
        assert!(!empty.apply(&ProjectMcpAction::Reset).authority_reduced);
        let all = empty.apply(&ProjectMcpAction::ApproveAll).choices;
        assert!(all.enable_all);
        let reset = all.apply(&ProjectMcpAction::Reset);
        assert!(reset.authority_reduced);
        assert_eq!(reset.choices, ProjectMcpChoices::default());
    }

    #[test]
    fn native_merge_keeps_the_primary_whole_entry() {
        let profile = vec![McpServerConfig::stdio("same", "profile", Vec::new())];
        let workspace_entry = |name: &str| McpServerConfig {
            source: ConfigSource::Workspace,
            scope: ConfigScope::Workspace,
            workspace_admission: Some(WorkspaceAdmission::Approved),
            ..McpServerConfig::stdio(name, "workspace", Vec::new())
        };
        let merged = merge_native(
            profile,
            vec![workspace_entry("same"), workspace_entry("only")],
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].command.as_deref(), Some("profile"));
        assert_eq!(merged[1].name, "only");
    }

    #[test]
    fn remote_config_keeps_credential_references_and_oauth_policy_without_secrets() {
        let result = parse_profile_document(
            br#"{"mcp":{"api":{"type":"http","url":"https://api.example.com/mcp","header_env":{"X-Org":"ORG_ENV"},"bearer_token_env":"MCP_TOKEN","oauth":{"resource":"https://api.example.com/mcp","client_id":"client","client_secret_env":"SECRET_ENV","scopes":[],"callback_port":3118}}}}"#,
        )
        .unwrap();
        let config = &result.configs[0];
        assert!(config.allow_stored_credentials);
        assert_eq!(config.header_env[0].env, "ORG_ENV");
        assert_eq!(config.bearer_token_env.as_deref(), Some("MCP_TOKEN"));
        let auth = config.auth.as_ref().unwrap();
        assert!(auth.scopes_configured);
        assert_eq!(auth.callback_port, Some(3118));
        assert_eq!(auth.client_secret_env.as_deref(), Some("SECRET_ENV"));
    }

    #[test]
    fn profile_config_rejects_invalid_remote_credentials() {
        let cases: [(&str, McpConfigError); 6] = [
            (
                r#"{"mcp":{"a":{"type":"http","url":"https://e.test/mcp","oauth":{"callback_port":0}}}}"#,
                McpConfigError::McpConfigInvalidOAuth,
            ),
            (
                r#"{"mcp":{"a":{"type":"http","url":"https://e.test/mcp","oauth":{"callback_port":"3118"}}}}"#,
                McpConfigError::McpConfigInvalidOAuth,
            ),
            (
                r#"{"mcp":{"a":{"type":"http","url":"https://e.test/mcp","oauth":{"client_metadata_url":"https://e.test/"}}}}"#,
                McpConfigError::McpConfigInvalidOAuth,
            ),
            (
                r#"{"mcp":{"a":{"type":"http","url":"https://e.test/mcp","oauth":{"client_secret_env":"SECRET"}}}}"#,
                McpConfigError::McpConfigInvalidOAuth,
            ),
            (
                r#"{"mcp":{"a":{"type":"http","url":"https://e.test/mcp","bearer_token_env":"1BAD"}}}"#,
                McpConfigError::McpConfigInvalidBearerEnvironment,
            ),
            (
                r#"{"mcp":{"a":{"type":"http","url":"http://example.com/mcp"}}}"#,
                McpConfigError::McpConfigInvalidUrl,
            ),
        ];
        for (json, expected) in cases {
            assert_eq!(
                parse_profile_document(json.as_bytes()),
                Err(expected),
                "{json}"
            );
        }
    }

    #[test]
    fn load_config_parses_env_alias_only_when_environment_is_absent() {
        let alias =
            parse_profile_document(br#"{"mcp":{"a":{"command":"node","env":{"A":"B"}}}}"#).unwrap();
        assert_eq!(alias.configs[0].env[0].value, "B");
        let preferred = parse_profile_document(
            br#"{"mcp":{"a":{"command":"node","environment":{"A":"one"},"env":{"A":"two"}}}}"#,
        )
        .unwrap();
        assert_eq!(preferred.configs[0].env[0].value, "one");
        assert_eq!(
            parse_profile_document(br#"{"mcp":{"a":{"command":"node","environment":{"A":1}}}}"#),
            Err(McpConfigError::McpConfigInvalidEnvironment)
        );
    }
}
