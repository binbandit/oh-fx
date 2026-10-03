mod command_provider;
mod mcp_contract;
mod native_config;
mod profile_store;
mod project_config;
mod settings_choices;
mod streamable_http;
mod uri;
mod workspace_config;

pub use command_provider::{AddIntent, AddIntentError, is_valid_server_name, parse_add_intent};
pub use mcp_contract::{
    ConfigScope, ConfigSource, DEFAULT_OPERATION_TIMEOUT_MS, DEFAULT_RESTART_LIMIT,
    DEFAULT_STARTUP_TIMEOUT_MS, EnvVar, HttpHeader, HttpHeaderEnv, InvalidServerConfig,
    McpAuthConfig, McpServerConfig, ProfileConfigWarning, ProfileConfigWarningCause, TransportType,
    WorkspaceAdmission,
};
pub use native_config::{NativeConfigLoad, load_native_configs};
pub use profile_store::{
    PROFILE_CONFIG_FILE_NAME, ProfileRemoveOutcome, ProfileStoreError, add_profile_server,
    load_profile_document, profile_config_path, remove_profile_server, render_profile_config,
};
pub use project_config::{
    DISABLED_SERVERS_KEY, ENABLE_ALL_KEY, ENABLED_SERVERS_KEY, InvalidProjectMcpChoices,
    McpConfigError, ProfileParseResult, ProjectMcpAction, ProjectMcpChoices, ProjectMcpTransition,
    WorkspaceDiagnostic, WorkspaceDiagnosticCause, WorkspaceEnvironmentField, WorkspaceParseResult,
    expand_approved_workspace_configs, merge_native, parse_profile_document,
    parse_workspace_document,
};
pub use settings_choices::{ProjectMcpSettingsChange, apply_project_mcp_action};
pub use streamable_http::{EndpointError, HeaderError, validate_endpoint, validate_static_headers};
pub use workspace_config::{
    WORKSPACE_CONFIG_FILE_NAME, load_workspace_config, load_workspace_config_with_environment,
};
