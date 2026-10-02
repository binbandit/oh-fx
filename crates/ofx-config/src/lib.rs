mod config_runtime;
mod configured_provider;
mod connection;
mod context_limits;
mod header_template;
mod io;
mod model_capabilities;
mod model_provider;
mod paths;
mod settings_store;
mod strict_json;

pub use config_runtime::{
    ConfigDiagnostic, LayerError, SelectionError, Settings, SettingsError,
    is_valid_provider_order_list,
};
pub use configured_provider::{
    ConfiguredProviderError, MAX_MODEL_BYTES, MaxTokensParameter, ProviderDefinition,
    ToolChoiceMode, is_valid_model_id,
};
pub use connection::{ConnectionError, ResolvedConnection};
pub use context_limits::{
    ContextLimit, ContextLimitError, ContextLimitName, ContextLimitOverride, ContextLimitSource,
    ContextLimitValue, ContextLimits, EMERGENCY_CEILING_BYTES, line_safe_prefix_length,
    parse_context_limit_override, utf8_prefix_length,
};
pub use io::{AdvisoryLock, DurableError, PrivateDir, RemoveOutcome};
pub use model_capabilities::{Capabilities, request_output_tokens};
pub use model_provider::{ProviderId, is_valid_provider_id};
pub use paths::ProfilePaths;
pub use settings_store::{
    LegacyCleanup, SettingsWriteError, SettingsWriteFailure, save_codex_model,
    save_model_preference, save_permission_mode, save_yolo_acknowledged,
};
pub use strict_json::{StrictJsonError, parse as parse_strict_json};
