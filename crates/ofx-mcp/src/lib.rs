mod catalog_freshness;
mod command_provider;
mod docker_run;
mod error;
mod feature_catalog;
mod feature_catalog_runtime;
mod feature_operations;
mod features;
mod health;
mod json_number;
mod legacy_elicitation_runtime;
mod legacy_http_sse;
mod legacy_sse;
mod legacy_streamable_http;
mod local_inspection;
mod mcp_contract;
mod mcp_json;
mod mcp_runtime;
mod native_config;
mod operation_control;
mod profile_store;
mod project_config;
mod protocol_messages;
mod protocol_negotiation;
mod server_auth;
mod server_connection;
mod server_lifecycle;
mod server_transport;
mod server_views;
mod settings_choices;
mod startup_admission;
mod stdio_dispatcher;
mod streamable_http;
#[cfg(test)]
mod test_support;
mod timing;
mod tool_mcp_registry;
mod tool_names;
mod tool_operations;
mod tool_result;
mod transport;
mod uri;
mod workspace_config;

pub use command_provider::{AddIntent, AddIntentError, is_valid_server_name, parse_add_intent};
pub use error::McpError;
pub use feature_operations::{PromptSummary, ResourceSummary};
pub use features::prompts::PromptArgument;
pub use local_inspection::{
    ConfiguredServer, LocalConfigInspection, ProfileConfigDiagnostic, inspect_local_config,
};
pub use mcp_contract::{
    ConfigScope, ConfigSource, DEFAULT_OPERATION_TIMEOUT_MS, DEFAULT_RESTART_LIMIT,
    DEFAULT_STARTUP_TIMEOUT_MS, EnvVar, HttpHeader, HttpHeaderEnv, InvalidServerConfig,
    McpAuthConfig, McpServerConfig, ProfileConfigWarning, ProfileConfigWarningCause, TransportType,
    WorkspaceAdmission,
};
pub use mcp_runtime::{McpRuntime, ReloadCancelled, ReloadOutcome, Settling};
pub use native_config::{NativeConfigLoad, load_native_configs, preview_workspace_authority};
pub use profile_store::{
    PROFILE_CONFIG_FILE_NAME, ProfileRemoveOutcome, ProfileStoreError, add_profile_server,
    load_profile_document, profile_config_path, remove_profile_server, render_profile_config,
};
pub use project_config::{
    DISABLED_SERVERS_KEY, ENABLE_ALL_KEY, ENABLED_SERVERS_KEY, InvalidProjectMcpChoices,
    McpConfigError, ProfileParseResult, ProjectMcpAction, ProjectMcpChoices, ProjectMcpTransition,
    WorkspaceDiagnostic, WorkspaceDiagnosticCause, WorkspaceEnvironmentField, WorkspaceParseResult,
    expand_approved_workspace_configs, merge_native, parse_profile_document,
    parse_workspace_document, render_workspace_diagnostic,
};
pub use server_transport::ConnectOptions;
pub use settings_choices::{ProjectMcpSettingsChange, apply_project_mcp_action_to_entry};
pub use startup_admission::{StartupDecision, StartupPhase, decide_startup};
pub use streamable_http::{EndpointError, HeaderError, validate_endpoint, validate_static_headers};
pub use tool_mcp_registry::SchemaLimits;
pub use transport::ShutdownMode;
pub use workspace_config::{
    WORKSPACE_CONFIG_FILE_NAME, load_workspace_config, load_workspace_config_with_environment,
};
