use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

use ofx_contract::{AutoCompactPercent, PermissionMode, ReasoningEffort};
use serde_json::{Map, Value};

use crate::configured_provider::{
    ConfiguredProviderError, ProviderDefinition, ProviderRegistry, validate_model_id,
};
use crate::context_limits::{
    ContextLimitError, ContextLimitOverrides, ContextLimitSource, ContextLimits,
};
use crate::model_provider::ProviderId;
use crate::paths::ProfilePaths;
use crate::settings_store::{MAX_PROVIDER_ORDER_ENTRIES, validate_provider_slug};
use crate::strict_json;

pub(crate) const SETTINGS_FILE: &str = "settings.json";
const PROJECT_FILE: &str = ".oh-fx.json";
pub(crate) const MAX_SETTINGS_BYTES: usize = 64 * 1024;
const MAX_MODEL_PREFERENCES: usize = 35;
const PROVIDER_VARIABLE: &str = "OH_FX_PROVIDER";
const MODEL_VARIABLE: &str = "OH_FX_MODEL";
const MAX_AGENT_STEPS_VARIABLE: &str = "OH_FX_MAX_AGENT_STEPS";
const AUTO_COMPACT_PERCENT_VARIABLE: &str = "OH_FX_AUTO_COMPACT_PERCENT";
pub(crate) const BYTE_ORDER_MARK: &[u8] = b"\xef\xbb\xbf";
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];
const PROFILE_ONLY_KEYS: [&str; 29] = [
    "model",
    "models",
    "provider",
    "providers",
    "codex_model",
    "grok_model",
    "review_model",
    "effort",
    "fast_mode",
    "fast_mode_model_bound",
    "slash_menu_categories",
    "collapse_tool_calls",
    "theme",
    "session_titles",
    "startup_scrollback",
    "prompt_history",
    "statusLine",
    "notifications",
    "context_limits",
    "skill_match_fuzzy",
    "first_call_tool_choice",
    "auto_upgrade",
    "update_channel",
    "permission_mode",
    "credential_source",
    "yolo_acknowledged",
    "permission",
    "additional_directories",
    "skill_symlink_authorities",
];
const MODEL_NOT_SELECTED: &str = "no model is selected for this connection; save one under \"models\" in ~/.config/oh-fx/settings.json, or set a model for this run with --model or OH_FX_MODEL";
const CONTEXT_LIMITS_REPAIR: &str = "; context_limits keys must be documented limit names with a non-negative integer or \"off\" value";
const CODEX_MODEL_NOT_SELECTED: &str = "no Codex model is selected; run `oh-fx provider codex` to choose one, or set a model for this run with --model or OH_FX_MODEL";

type EnvironmentLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigLayer {
    User,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnosticCause {
    MalformedSettings,
    SettingsTooLarge,
    DurablePathUnsafe,
    InvalidModelId,
    IgnoredProjectUserOnlySetting,
    InvalidContextLimits,
}

impl DiagnosticCause {
    const fn label(self) -> &'static str {
        match self {
            Self::MalformedSettings => "malformed_settings",
            Self::SettingsTooLarge => "settings_too_large",
            Self::DurablePathUnsafe => "durable_path_unsafe",
            Self::InvalidModelId => "invalid_model_id",
            Self::IgnoredProjectUserOnlySetting => "ignored_project_user_only_setting",
            Self::InvalidContextLimits => "invalid_context_limits",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    layer: ConfigLayer,
    cause: DiagnosticCause,
    key: Option<String>,
}

impl fmt::Display for ConfigDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let layer = match self.layer {
            ConfigLayer::User => "user",
            ConfigLayer::Project => "project",
        };
        write!(formatter, "config {layer}: {}", self.cause.label())?;
        if let Some(key) = &self.key {
            write!(formatter, "; key={key}")?;
        }
        if self.cause == DiagnosticCause::InvalidContextLimits {
            formatter.write_str(CONTEXT_LIMITS_REPAIR)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    #[error("{0}")]
    Providers(ConfiguredProviderError),
    #[error("{0}")]
    Layer(LayerError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LayerError {
    #[error("InvalidModelType")]
    InvalidModelType,
    #[error("InvalidModelValue")]
    InvalidModelValue,
    #[error("InvalidProviderType")]
    InvalidProviderType,
    #[error("InvalidProviderValue")]
    InvalidProviderValue,
    #[error("InvalidCodexModelType")]
    InvalidCodexModelType,
    #[error("InvalidCodexModelValue")]
    InvalidCodexModelValue,
    #[error("TooManyModelPreferences")]
    TooManyModelPreferences,
    #[error("InvalidPermissionModeType")]
    InvalidPermissionModeType,
    #[error("InvalidPermissionMode")]
    InvalidPermissionMode,
    #[error("InvalidYoloAcknowledgedType")]
    InvalidYoloAcknowledgedType,
    #[error("InvalidMaxAgentStepsType")]
    InvalidMaxAgentStepsType,
    #[error("InvalidMaxAgentStepsValue")]
    InvalidMaxAgentStepsValue,
    #[error("InvalidEffortType")]
    InvalidEffortType,
    #[error("InvalidEffortValue")]
    InvalidEffortValue,
    #[error("InvalidFastModeType")]
    InvalidFastModeType,
    #[error("InvalidFastModeBindingType")]
    InvalidFastModeBindingType,
    #[error("InvalidAutoCompactPercentType")]
    InvalidAutoCompactPercentType,
    #[error("InvalidAutoCompactPercentValue")]
    InvalidAutoCompactPercentValue,
    #[error("{0}")]
    ContextLimits(ContextLimitError),
    #[error("InvalidContextType")]
    InvalidContextType,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectionError {
    #[error("InvalidProviderValue")]
    InvalidProviderValue,
    #[error("UnknownConfiguredProvider")]
    UnknownConfiguredProvider,
    #[error("ConfiguredProviderChanged")]
    ConfiguredProviderChanged,
    #[error(
        "the {0} provider is not available in oh-fx yet; add a connection under \"providers\" in ~/.config/oh-fx/settings.json and select it with \"provider\" or OH_FX_PROVIDER"
    )]
    ProviderUnavailable(String),
    #[error("{MODEL_NOT_SELECTED}")]
    ModelNotSelected,
    #[error("{CODEX_MODEL_NOT_SELECTED}")]
    CodexModelNotSelected,
}

impl SelectionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidProviderValue => "InvalidProviderValue",
            Self::UnknownConfiguredProvider => "UnknownConfiguredProvider",
            Self::ConfiguredProviderChanged => "ConfiguredProviderChanged",
            Self::ProviderUnavailable(_) => "ProviderUnavailable",
            Self::ModelNotSelected => "ConfiguredModelNotSelected",
            Self::CodexModelNotSelected => "CodexModelNotSelected",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Layer {
    provider: Option<String>,
    model: Option<String>,
    codex_model: Option<String>,
    models: Vec<(ProviderId, String)>,
    permission_mode: Option<PermissionMode>,
    yolo_acknowledged: Option<bool>,
    max_agent_steps: Option<u64>,
    auto_compact_percent: Option<AutoCompactPercent>,
    effort: Option<ReasoningEffort>,
    fast_mode: Option<bool>,
    context_limits: ContextLimitOverrides,
    context: Option<bool>,
}

impl Layer {
    fn codex_model(&self) -> Option<&str> {
        self.models
            .iter()
            .find(|(id, _)| *id == ProviderId::Codex)
            .map(|(_, model)| model.as_str())
            .or(self.codex_model.as_deref())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    providers: ProviderRegistry,
    global: Layer,
    workspace: Layer,
    resumed: Layer,
    project_max_agent_steps: Option<u64>,
    project_context: Option<bool>,
    diagnostics: Vec<ConfigDiagnostic>,
}

impl From<LayerError> for DiagnosticCause {
    fn from(error: LayerError) -> Self {
        match error {
            LayerError::InvalidModelValue => Self::InvalidModelId,
            LayerError::ContextLimits(_) => Self::InvalidContextLimits,
            _ => Self::MalformedSettings,
        }
    }
}

impl Settings {
    pub fn load(paths: &ProfilePaths, workspace_root: &Path) -> Result<Self, SettingsError> {
        let mut settings = Self::default();
        settings.load_project(&workspace_root.join(PROJECT_FILE));
        if let Some(profile) =
            settings.read_object(&paths.config.join(SETTINGS_FILE), ConfigLayer::User)
        {
            settings.apply_profile(&profile, workspace_root)?;
        }
        Ok(settings)
    }

    pub fn diagnostics(&self) -> &[ConfigDiagnostic] {
        &self.diagnostics
    }

    pub fn profile_is_unusable(&self) -> bool {
        self.diagnostics.iter().any(|diagnostic| {
            diagnostic.layer == ConfigLayer::User
                && matches!(
                    diagnostic.cause,
                    DiagnosticCause::MalformedSettings
                        | DiagnosticCause::SettingsTooLarge
                        | DiagnosticCause::InvalidModelId
                        | DiagnosticCause::DurablePathUnsafe
                )
        })
    }

    pub fn permission_mode(&self) -> PermissionMode {
        self.workspace
            .permission_mode
            .or(self.global.permission_mode)
            .unwrap_or(PermissionMode::Auto)
    }

    pub fn yolo_acknowledged(&self) -> bool {
        self.workspace
            .yolo_acknowledged
            .or(self.global.yolo_acknowledged)
            .unwrap_or(false)
    }

    pub fn reasoning_effort(&self) -> ReasoningEffort {
        self.workspace
            .effort
            .as_ref()
            .or(self.global.effort.as_ref())
            .cloned()
            .unwrap_or(ReasoningEffort::Auto)
    }

    pub fn fast_mode(&self) -> bool {
        self.workspace
            .fast_mode
            .or(self.global.fast_mode)
            .unwrap_or(false)
    }

    pub fn context_enabled(&self) -> bool {
        self.workspace
            .context
            .or(self.global.context)
            .or(self.project_context)
            .unwrap_or(true)
    }

    pub fn context_limits(&self) -> ContextLimits {
        let mut limits = ContextLimits::default();
        limits.apply(
            &self.global.context_limits,
            ContextLimitSource::GlobalSettings,
        );
        limits.apply(
            &self.workspace.context_limits,
            ContextLimitSource::WorkspaceSettings,
        );
        limits
    }

    pub fn resume_selection(
        &mut self,
        provider: &ProviderId,
        binding: Option<[u8; 32]>,
        model: &str,
        lookup: EnvironmentLookup<'_>,
    ) -> Result<(), SelectionError> {
        if environment_provider(lookup).is_some() {
            return Ok(());
        }
        if let ProviderId::Configured(id) = provider {
            let definition = self
                .providers
                .get(id)
                .ok_or(SelectionError::UnknownConfiguredProvider)?;
            if binding.is_some_and(|binding| binding != definition.binding_identity()) {
                return Err(SelectionError::ConfiguredProviderChanged);
            }
        }
        self.resumed = Layer {
            provider: Some(provider.label().to_owned()),
            models: vec![(provider.clone(), model.to_owned())],
            ..Layer::default()
        };
        Ok(())
    }

    pub(crate) fn selected_provider(
        &self,
        lookup: EnvironmentLookup<'_>,
    ) -> Result<ProviderId, SelectionError> {
        let raw = environment_provider(lookup)
            .or_else(|| self.resumed.provider.clone())
            .or_else(|| self.workspace.provider.clone())
            .or_else(|| self.global.provider.clone());
        let Some(raw) = raw else {
            return Ok(ProviderId::Gateway);
        };
        let provider = ProviderId::parse(&raw).ok_or(SelectionError::InvalidProviderValue)?;
        if let ProviderId::Configured(id) = &provider
            && self.providers.get(id).is_none()
        {
            return Err(SelectionError::UnknownConfiguredProvider);
        }
        Ok(provider)
    }

    pub fn codex_selected(&self, lookup: EnvironmentLookup<'_>) -> Result<bool, SelectionError> {
        Ok(self.selected_provider(lookup)? == ProviderId::Codex)
    }

    pub fn selected_codex_model(
        &self,
        run_model: Option<&str>,
        lookup: EnvironmentLookup<'_>,
    ) -> Result<String, SelectionError> {
        run_model
            .map(str::to_owned)
            .or_else(|| environment_model(lookup))
            .or_else(|| self.resumed.codex_model().map(str::to_owned))
            .or_else(|| self.saved_codex_model().map(str::to_owned))
            .ok_or(SelectionError::CodexModelNotSelected)
    }

    pub fn saved_codex_model(&self) -> Option<&str> {
        self.workspace
            .codex_model()
            .or_else(|| self.global.codex_model())
    }

    pub fn selected_connection(
        &self,
        lookup: EnvironmentLookup<'_>,
    ) -> Result<&ProviderDefinition, SelectionError> {
        match self.selected_provider(lookup)? {
            ProviderId::Configured(id) => self
                .providers
                .get(&id)
                .ok_or(SelectionError::UnknownConfiguredProvider),
            builtin => Err(SelectionError::ProviderUnavailable(
                builtin.label().to_owned(),
            )),
        }
    }

    pub fn selected_model(
        &self,
        connection: &ProviderDefinition,
        run_model: Option<&str>,
        lookup: EnvironmentLookup<'_>,
    ) -> Result<String, SelectionError> {
        let provider = ProviderId::Configured(connection.id.clone());
        let saved = |layer: &Layer| {
            layer
                .models
                .iter()
                .find(|(id, _)| *id == provider)
                .map(|(_, model)| model.clone())
                .or_else(|| layer.model.clone())
        };
        run_model
            .map(str::to_owned)
            .or_else(|| environment_model(lookup))
            .or_else(|| saved(&self.resumed))
            .or_else(|| saved(&self.workspace))
            .or_else(|| saved(&self.global))
            .or_else(|| connection.models.first().cloned())
            .ok_or(SelectionError::ModelNotSelected)
    }

    pub fn max_agent_steps(&self, lookup: EnvironmentLookup<'_>) -> u64 {
        lookup(MAX_AGENT_STEPS_VARIABLE)
            .and_then(|value| value.trim().parse().ok())
            .or(self.workspace.max_agent_steps)
            .or(self.global.max_agent_steps)
            .or(self.project_max_agent_steps)
            .unwrap_or(0)
    }

    pub fn auto_compact_percent(&self, lookup: EnvironmentLookup<'_>) -> AutoCompactPercent {
        AutoCompactPercent::resolve(
            self.workspace
                .auto_compact_percent
                .or(self.global.auto_compact_percent),
            lookup(AUTO_COMPACT_PERCENT_VARIABLE).as_deref(),
        )
    }

    fn load_project(&mut self, path: &Path) {
        let Some(project) = self.read_object(path, ConfigLayer::Project) else {
            return;
        };
        for key in project.keys() {
            if PROFILE_ONLY_KEYS.contains(&key.as_str()) {
                self.diagnose(
                    ConfigLayer::Project,
                    DiagnosticCause::IgnoredProjectUserOnlySetting,
                    Some(key.clone()),
                );
            }
        }
        let parsed = parse_steps(&project).and_then(|steps| {
            parse_switch(&project, "context", LayerError::InvalidContextType)
                .map(|context| (steps, context))
        });
        match parsed {
            Ok((steps, context)) => {
                self.project_max_agent_steps = steps;
                self.project_context = context;
            }
            Err(failure) => self.diagnose(ConfigLayer::Project, failure.into(), None),
        }
    }

    fn apply_profile(
        &mut self,
        profile: &Map<String, Value>,
        workspace_root: &Path,
    ) -> Result<(), SettingsError> {
        if let Some(providers) = profile.get("providers") {
            self.providers =
                ProviderRegistry::parse(providers).map_err(SettingsError::Providers)?;
        }
        self.global = self.parse_profile_layer(profile)?;
        let workspace_key = workspace_root.to_string_lossy();
        let workspace = match profile.get("workspaces") {
            None => None,
            Some(Value::Object(workspaces)) => match workspaces.get(workspace_key.as_ref()) {
                None => None,
                Some(Value::Object(entry)) => Some(entry),
                Some(_) => {
                    self.diagnose(ConfigLayer::User, DiagnosticCause::MalformedSettings, None);
                    None
                }
            },
            Some(_) => {
                self.diagnose(ConfigLayer::User, DiagnosticCause::MalformedSettings, None);
                None
            }
        };
        if let Some(entry) = workspace {
            self.workspace = self.parse_profile_layer(entry)?;
        }
        Ok(())
    }

    fn parse_profile_layer(&mut self, object: &Map<String, Value>) -> Result<Layer, SettingsError> {
        match parse_layer(object) {
            Ok(layer) => Ok(layer),
            Err(error) => {
                self.diagnose(ConfigLayer::User, error.into(), None);
                parse_routing(object).map_err(|_| SettingsError::Layer(error))
            }
        }
    }

    fn read_object(&mut self, path: &Path, layer: ConfigLayer) -> Option<Map<String, Value>> {
        let bytes = match read_bounded(path) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return None,
            Err(cause) => {
                self.diagnose(layer, cause, None);
                return None;
            }
        };
        let bytes = bytes.strip_prefix(BYTE_ORDER_MARK).unwrap_or(&bytes);
        if let Ok(Value::Object(object)) = strict_json::parse(bytes) {
            return Some(object);
        }
        self.diagnose(layer, DiagnosticCause::MalformedSettings, None);
        None
    }

    fn diagnose(&mut self, layer: ConfigLayer, cause: DiagnosticCause, key: Option<String>) {
        self.diagnostics
            .push(ConfigDiagnostic { layer, cause, key });
    }
}

pub fn is_valid_provider_order_list(raw: &str) -> bool {
    let mut slugs: Vec<&str> = Vec::new();
    for slug in raw.split(',').map(|token| token.trim_matches(TRIMMED)) {
        if slug.is_empty() {
            continue;
        }
        if !validate_provider_slug(slug)
            || slugs.len() >= MAX_PROVIDER_ORDER_ENTRIES
            || slugs.contains(&slug)
        {
            return false;
        }
        slugs.push(slug);
    }
    !slugs.is_empty()
}

fn environment_provider(lookup: EnvironmentLookup<'_>) -> Option<String> {
    lookup(PROVIDER_VARIABLE).filter(|value| !value.trim().is_empty())
}

fn environment_model(lookup: EnvironmentLookup<'_>) -> Option<String> {
    lookup(MODEL_VARIABLE)
        .map(|model| model.trim().to_owned())
        .filter(|model| !model.is_empty())
}

fn read_bounded(path: &Path) -> Result<Option<Vec<u8>>, DiagnosticCause> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(DiagnosticCause::DurablePathUnsafe),
    };
    let metadata = file
        .metadata()
        .map_err(|_| DiagnosticCause::DurablePathUnsafe)?;
    if !metadata.is_file() {
        return Err(DiagnosticCause::DurablePathUnsafe);
    }
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| DiagnosticCause::DurablePathUnsafe)?;
    if bytes.len() > MAX_SETTINGS_BYTES {
        return Err(DiagnosticCause::SettingsTooLarge);
    }
    Ok(Some(bytes))
}

pub(crate) fn is_valid_profile_layer(object: &Map<String, Value>) -> bool {
    parse_layer(object).is_ok()
}

fn parse_layer(object: &Map<String, Value>) -> Result<Layer, LayerError> {
    let model = object.get("model").map(parse_model).transpose()?;
    let mut layer = parse_routing(object)?;
    layer.model = model;
    layer.permission_mode = object
        .get("permission_mode")
        .map(|value| {
            let Value::String(mode) = value else {
                return Err(LayerError::InvalidPermissionModeType);
            };
            PermissionMode::parse(mode).ok_or(LayerError::InvalidPermissionMode)
        })
        .transpose()?;
    layer.yolo_acknowledged = parse_switch(
        object,
        "yolo_acknowledged",
        LayerError::InvalidYoloAcknowledgedType,
    )?;
    layer.context_limits = object
        .get("context_limits")
        .map(ContextLimitOverrides::parse_json)
        .transpose()
        .map_err(LayerError::ContextLimits)?
        .unwrap_or_default();
    layer.max_agent_steps = parse_steps(object)?;
    layer.auto_compact_percent = object
        .get("auto_compact_percent")
        .map(parse_auto_compact_percent)
        .transpose()?;
    layer.effort = object.get("effort").map(parse_effort).transpose()?;
    layer.fast_mode = parse_switch(object, "fast_mode", LayerError::InvalidFastModeType)?;
    parse_switch(
        object,
        "fast_mode_model_bound",
        LayerError::InvalidFastModeBindingType,
    )?;
    layer.context = parse_switch(object, "context", LayerError::InvalidContextType)?;
    Ok(layer)
}

fn parse_switch(
    object: &Map<String, Value>,
    key: &str,
    invalid: LayerError,
) -> Result<Option<bool>, LayerError> {
    object
        .get(key)
        .map(|value| value.as_bool().ok_or(invalid))
        .transpose()
}

fn parse_effort(value: &Value) -> Result<ReasoningEffort, LayerError> {
    match value {
        Value::String(raw) => ReasoningEffort::parse(raw).ok_or(LayerError::InvalidEffortValue),
        Value::Null => Ok(ReasoningEffort::Auto),
        _ => Err(LayerError::InvalidEffortType),
    }
}

fn parse_routing(object: &Map<String, Value>) -> Result<Layer, LayerError> {
    let provider = match object.get("provider") {
        None => None,
        Some(Value::String(provider)) => Some(provider.clone()),
        Some(_) => return Err(LayerError::InvalidProviderType),
    };
    let codex_model = match object.get("codex_model") {
        None => None,
        Some(Value::String(model)) => {
            validate_model_id(model).map_err(|_| LayerError::InvalidCodexModelValue)?;
            Some(model.clone())
        }
        Some(_) => return Err(LayerError::InvalidCodexModelType),
    };
    let models = match object.get("models") {
        None => Vec::new(),
        Some(Value::Object(models)) => {
            if models.len() > MAX_MODEL_PREFERENCES {
                return Err(LayerError::TooManyModelPreferences);
            }
            models
                .iter()
                .map(|(provider, model)| {
                    let provider =
                        ProviderId::parse(provider).ok_or(LayerError::InvalidProviderValue)?;
                    Ok((provider, parse_model(model)?))
                })
                .collect::<Result<_, LayerError>>()?
        }
        Some(_) => return Err(LayerError::InvalidModelType),
    };
    Ok(Layer {
        provider,
        codex_model,
        models,
        ..Layer::default()
    })
}

fn parse_model(value: &Value) -> Result<String, LayerError> {
    let Value::String(model) = value else {
        return Err(LayerError::InvalidModelType);
    };
    validate_model_id(model).map_err(|_| LayerError::InvalidModelValue)?;
    Ok(model.clone())
}

fn parse_steps(object: &Map<String, Value>) -> Result<Option<u64>, LayerError> {
    object
        .get("max_agent_steps")
        .map(|steps| match steps {
            Value::Number(number) if number.is_u64() => {
                number.as_u64().ok_or(LayerError::InvalidMaxAgentStepsValue)
            }
            Value::Number(number) if number.is_i64() => Err(LayerError::InvalidMaxAgentStepsValue),
            _ => Err(LayerError::InvalidMaxAgentStepsType),
        })
        .transpose()
}

fn parse_auto_compact_percent(value: &Value) -> Result<AutoCompactPercent, LayerError> {
    let Value::Number(number) = value else {
        return Err(LayerError::InvalidAutoCompactPercentType);
    };
    let integer = number
        .as_i64()
        .ok_or(LayerError::InvalidAutoCompactPercentType)?;
    u64::try_from(integer)
        .ok()
        .and_then(AutoCompactPercent::new)
        .ok_or(LayerError::InvalidAutoCompactPercentValue)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::context_limits::{ContextLimitName, EMERGENCY_CEILING_BYTES};

    const PORTKEY_SETTINGS: &str = r#"{"provider":"portkey","model":"@openai/gpt-4o","providers":{"portkey":{"protocol":"openai-chat-completions","base_url":"https://portkey.internal.example.com/v1","auth":{"type":"none"},"headers":{"x-portkey-api-key":"${PORTKEY_API_KEY}"},"models":["@openai/gpt-4o"]}}}"#;

    struct Fixture {
        _directory: tempfile::TempDir,
        paths: ProfilePaths,
        workspace: PathBuf,
    }

    fn fixture(settings: Option<&str>, project: Option<&str>) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config/oh-fx");
        let workspace = directory.path().join("workspace");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        if let Some(settings) = settings {
            fs::write(config.join(SETTINGS_FILE), settings).unwrap();
        }
        if let Some(project) = project {
            fs::write(workspace.join(PROJECT_FILE), project).unwrap();
        }
        let paths = ProfilePaths {
            config,
            data: directory.path().join("data"),
            state: directory.path().join("state"),
            cache: directory.path().join("cache"),
        };
        Fixture {
            _directory: directory,
            paths,
            workspace,
        }
    }

    fn load(fixture: &Fixture) -> Result<Settings, SettingsError> {
        Settings::load(&fixture.paths, &fixture.workspace)
    }

    fn no_environment(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn selects_the_portkey_connection_and_model_from_settings() {
        let fixture = fixture(Some(PORTKEY_SETTINGS), None);
        let settings = load(&fixture).unwrap();
        assert!(settings.diagnostics().is_empty());
        let connection = settings.selected_connection(&no_environment).unwrap();
        assert_eq!(connection.id, "portkey");
        assert_eq!(
            settings.selected_model(connection, None, &no_environment),
            Ok("@openai/gpt-4o".to_owned())
        );
    }

    #[test]
    fn model_precedence_runs_from_flag_to_environment_to_saved_to_listed() {
        let settings_json = r#"{"provider":"local","models":{"local":"saved"},"providers":{"local":{"protocol":"openai-chat-completions","base_url":"http://localhost:1234/v1","auth":{"type":"none"},"models":["listed"]}}}"#;
        let fixture = fixture(Some(settings_json), None);
        let settings = load(&fixture).unwrap();
        let connection = settings.selected_connection(&no_environment).unwrap();
        let environment = |name: &str| (name == MODEL_VARIABLE).then(|| " from-env ".to_owned());
        assert_eq!(
            settings.selected_model(connection, Some("flag"), &environment),
            Ok("flag".to_owned())
        );
        assert_eq!(
            settings.selected_model(connection, None, &environment),
            Ok("from-env".to_owned())
        );
        assert_eq!(
            settings.selected_model(connection, None, &no_environment),
            Ok("saved".to_owned())
        );
        let listed_only = fixture_settings(
            r#"{"provider":"local","providers":{"local":{"protocol":"openai-chat-completions","base_url":"http://localhost:1234/v1","auth":{"type":"none"},"models":["listed"]}}}"#,
        );
        let connection = listed_only.selected_connection(&no_environment).unwrap();
        assert_eq!(
            listed_only.selected_model(connection, None, &no_environment),
            Ok("listed".to_owned())
        );
        let nothing = fixture_settings(
            r#"{"provider":"local","providers":{"local":{"protocol":"openai-chat-completions","base_url":"http://localhost:1234/v1","auth":{"type":"none"}}}}"#,
        );
        let connection = nothing.selected_connection(&no_environment).unwrap();
        let error = nothing
            .selected_model(connection, None, &no_environment)
            .unwrap_err();
        assert_eq!(error.to_string(), MODEL_NOT_SELECTED);
    }

    fn fixture_settings(json: &str) -> Settings {
        load(&fixture(Some(json), None)).unwrap()
    }

    #[test]
    fn environment_provider_overrides_settings_and_must_exist() {
        let settings = fixture_settings(PORTKEY_SETTINGS);
        let unknown = |name: &str| (name == PROVIDER_VARIABLE).then(|| "other".to_owned());
        assert_eq!(
            settings.selected_provider(&unknown),
            Err(SelectionError::UnknownConfiguredProvider)
        );
        let invalid = |name: &str| (name == PROVIDER_VARIABLE).then(|| "bad name".to_owned());
        assert_eq!(
            settings.selected_provider(&invalid),
            Err(SelectionError::InvalidProviderValue)
        );
        let gateway = |name: &str| (name == PROVIDER_VARIABLE).then(|| "Gateway".to_owned());
        assert_eq!(
            settings.selected_provider(&gateway),
            Ok(ProviderId::Gateway)
        );
        assert!(matches!(
            settings.selected_connection(&gateway),
            Err(SelectionError::ProviderUnavailable(label)) if label == "gateway"
        ));
        let blank = |name: &str| (name == PROVIDER_VARIABLE).then(|| "  ".to_owned());
        assert_eq!(
            settings.selected_provider(&blank),
            Ok(ProviderId::Configured("portkey".to_owned()))
        );
    }

    #[test]
    fn a_resumed_selection_wins_over_settings_but_not_over_the_environment() {
        let json = r#"{"provider":"one","models":{"one":"saved-one","two":"saved-two"},"providers":{"one":{"protocol":"openai-chat-completions","base_url":"http://localhost:1/v1","auth":{"type":"none"}},"two":{"protocol":"openai-chat-completions","base_url":"http://localhost:2/v1","auth":{"type":"bearer","env":"TWO_KEY"}}}}"#;
        let original = fixture_settings(json);
        let two = ProviderId::Configured("two".to_owned());
        let binding = original.providers.get("two").unwrap().binding_identity();
        let mut settings = original.clone();
        settings
            .resume_selection(&two, Some(binding), "resumed-model", &no_environment)
            .unwrap();
        let connection = settings.selected_connection(&no_environment).unwrap();
        assert_eq!(connection.id, "two");
        assert_eq!(
            settings.selected_model(connection, None, &no_environment),
            Ok("resumed-model".to_owned())
        );
        let model = |name: &str| (name == MODEL_VARIABLE).then(|| "env-model".to_owned());
        assert_eq!(
            settings.selected_model(connection, None, &model),
            Ok("env-model".to_owned())
        );
        assert_eq!(
            settings.selected_model(connection, Some("flag"), &no_environment),
            Ok("flag".to_owned())
        );
        let provider = |name: &str| (name == PROVIDER_VARIABLE).then(|| "one".to_owned());
        let mut overridden = original.clone();
        overridden
            .resume_selection(&two, Some(binding), "resumed-model", &provider)
            .unwrap();
        assert_eq!(overridden, original);
        let mut changed = original.clone();
        assert_eq!(
            changed.resume_selection(&two, Some([0; 32]), "m", &no_environment),
            Err(SelectionError::ConfiguredProviderChanged)
        );
        assert_eq!(
            changed.resume_selection(
                &ProviderId::Configured("gone".to_owned()),
                Some(binding),
                "m",
                &no_environment
            ),
            Err(SelectionError::UnknownConfiguredProvider)
        );
        assert_eq!(changed, original);
        let mut codex = original.clone();
        codex
            .resume_selection(&ProviderId::Codex, None, "gpt-5.4", &no_environment)
            .unwrap();
        assert_eq!(codex.codex_selected(&no_environment), Ok(true));
        assert_eq!(
            codex.selected_codex_model(None, &no_environment),
            Ok("gpt-5.4".to_owned())
        );
    }

    #[test]
    fn workspace_overrides_win_over_global_routing() {
        let fixture = fixture(None, None);
        let workspace_key = fixture.workspace.to_string_lossy().into_owned();
        let json = format!(
            r#"{{"provider":"one","providers":{{"one":{{"protocol":"openai-chat-completions","base_url":"http://localhost:1/v1","auth":{{"type":"none"}}}},"two":{{"protocol":"openai-chat-completions","base_url":"http://localhost:2/v1","auth":{{"type":"none"}}}}}},"workspaces":{{{}:{{"provider":"two","models":{{"two":"local-model"}},"max_agent_steps":4}}}}}}"#,
            serde_json::to_string(&workspace_key).unwrap()
        );
        fs::write(fixture.paths.config.join(SETTINGS_FILE), json).unwrap();
        let settings = load(&fixture).unwrap();
        let connection = settings.selected_connection(&no_environment).unwrap();
        assert_eq!(connection.id, "two");
        assert_eq!(
            settings.selected_model(connection, None, &no_environment),
            Ok("local-model".to_owned())
        );
        assert_eq!(settings.max_agent_steps(&no_environment), 4);
    }

    #[test]
    fn auto_compaction_percent_is_a_profile_setting_between_10_and_80() {
        let global = fixture_settings(r#"{"auto_compact_percent":50}"#);
        assert_eq!(global.auto_compact_percent(&no_environment).get(), 50);
        let environment = |value: &'static str| {
            move |name: &str| (name == AUTO_COMPACT_PERCENT_VARIABLE).then(|| value.to_owned())
        };
        assert_eq!(global.auto_compact_percent(&environment(" 25 ")).get(), 25);
        assert_eq!(global.auto_compact_percent(&environment("95")).get(), 50);
        for (json, error) in [
            (
                r#"{"auto_compact_percent":90}"#,
                LayerError::InvalidAutoCompactPercentValue,
            ),
            (
                r#"{"auto_compact_percent":5}"#,
                LayerError::InvalidAutoCompactPercentValue,
            ),
            (
                r#"{"auto_compact_percent":-1}"#,
                LayerError::InvalidAutoCompactPercentValue,
            ),
            (
                r#"{"auto_compact_percent":"50"}"#,
                LayerError::InvalidAutoCompactPercentType,
            ),
            (
                r#"{"auto_compact_percent":50.0}"#,
                LayerError::InvalidAutoCompactPercentType,
            ),
            (
                r#"{"auto_compact_percent":9223372036854775808}"#,
                LayerError::InvalidAutoCompactPercentType,
            ),
        ] {
            let object: Map<String, Value> = serde_json::from_str(json).unwrap();
            assert_eq!(parse_layer(&object), Err(error), "{json}");
            let settings = fixture_settings(json);
            assert_eq!(
                settings.diagnostics()[0].cause,
                DiagnosticCause::MalformedSettings
            );
            assert!(settings.profile_is_unusable());
        }
        let project = load(&fixture(None, Some(r#"{"auto_compact_percent":50}"#))).unwrap();
        assert!(project.diagnostics().is_empty());
        assert_eq!(project.auto_compact_percent(&no_environment).get(), 80);
    }

    #[test]
    fn workspace_auto_compaction_percent_wins_over_the_global_one() {
        let fixture = fixture(None, None);
        let workspace_key = fixture.workspace.to_string_lossy().into_owned();
        let json = format!(
            r#"{{"auto_compact_percent":60,"workspaces":{{{}:{{"auto_compact_percent":30}}}}}}"#,
            serde_json::to_string(&workspace_key).unwrap()
        );
        fs::write(fixture.paths.config.join(SETTINGS_FILE), json).unwrap();
        let settings = load(&fixture).unwrap();
        assert_eq!(settings.auto_compact_percent(&no_environment).get(), 30);
    }

    #[test]
    fn broken_profiles_are_diagnosed_and_block_runtime_use() {
        let malformed = load(&fixture(Some("{not json"), None)).unwrap();
        assert!(malformed.profile_is_unusable());
        assert_eq!(
            malformed.diagnostics()[0].to_string(),
            "config user: malformed_settings"
        );
        let duplicate = load(&fixture(Some(r#"{"provider":"a","provider":"b"}"#), None)).unwrap();
        assert!(duplicate.profile_is_unusable());
        let large = "x".repeat(MAX_SETTINGS_BYTES + 1);
        let too_large = load(&fixture(Some(&large), None)).unwrap();
        assert_eq!(
            too_large.diagnostics()[0].cause,
            DiagnosticCause::SettingsTooLarge
        );
        let bad_model = load(&fixture(Some(r#"{"model":" padded"}"#), None)).unwrap();
        assert_eq!(
            bad_model.diagnostics()[0].cause,
            DiagnosticCause::InvalidModelId
        );
        assert!(bad_model.profile_is_unusable());
    }

    #[test]
    fn invalid_non_routing_fields_keep_salvaged_routing() {
        let json = r#"{"provider":"local","max_agent_steps":"many","providers":{"local":{"protocol":"openai-chat-completions","base_url":"http://localhost:1234/v1","auth":{"type":"none"}}}}"#;
        let settings = fixture_settings(json);
        assert!(settings.profile_is_unusable());
        assert_eq!(
            settings.selected_provider(&no_environment),
            Ok(ProviderId::Configured("local".to_owned()))
        );
        assert_eq!(
            load(&fixture(Some(r#"{"provider":7}"#), None)),
            Err(SettingsError::Layer(LayerError::InvalidProviderType))
        );
    }

    #[test]
    fn unsalvageable_layers_fail_with_the_upstream_error_name() {
        let cases = [
            (r#"{"provider":7}"#, LayerError::InvalidProviderType),
            (r#"{"models":[]}"#, LayerError::InvalidModelType),
            (
                r#"{"models":{"bad name":"m"}}"#,
                LayerError::InvalidProviderValue,
            ),
            (r#"{"models":{"local":7}}"#, LayerError::InvalidModelType),
            (
                r#"{"models":{"local":" m"}}"#,
                LayerError::InvalidModelValue,
            ),
            (r#"{"model":7,"models":7}"#, LayerError::InvalidModelType),
            (r#"{"codex_model":7}"#, LayerError::InvalidCodexModelType),
            (
                r#"{"codex_model":" gpt"}"#,
                LayerError::InvalidCodexModelValue,
            ),
        ];
        for (json, error) in cases {
            let loaded = load(&fixture(Some(json), None));
            assert_eq!(loaded, Err(SettingsError::Layer(error)), "{json}");
            assert_eq!(loaded.unwrap_err().to_string(), error.to_string());
        }
        let too_many: Vec<String> = (0..=MAX_MODEL_PREFERENCES)
            .map(|index| format!(r#""p{index}":"m""#))
            .collect();
        let json = format!(r#"{{"models":{{{}}}}}"#, too_many.join(","));
        assert_eq!(
            load(&fixture(Some(&json), None)),
            Err(SettingsError::Layer(LayerError::TooManyModelPreferences))
        );
        for (json, cause) in [
            (
                r#"{"max_agent_steps":-1}"#,
                DiagnosticCause::MalformedSettings,
            ),
            (
                r#"{"max_agent_steps":1.5}"#,
                DiagnosticCause::MalformedSettings,
            ),
            (
                r#"{"permission_mode":"never"}"#,
                DiagnosticCause::MalformedSettings,
            ),
            (
                r#"{"yolo_acknowledged":"yes"}"#,
                DiagnosticCause::MalformedSettings,
            ),
            (r#"{"effort":42}"#, DiagnosticCause::MalformedSettings),
            (
                r#"{"effort":"very high"}"#,
                DiagnosticCause::MalformedSettings,
            ),
            (r#"{"fast_mode":"yes"}"#, DiagnosticCause::MalformedSettings),
            (
                r#"{"fast_mode_model_bound":1}"#,
                DiagnosticCause::MalformedSettings,
            ),
            (r#"{"model":" bad"}"#, DiagnosticCause::InvalidModelId),
        ] {
            let settings = load(&fixture(Some(json), None)).unwrap();
            assert_eq!(settings.diagnostics()[0].cause, cause, "{json}");
            assert!(settings.profile_is_unusable());
        }
    }

    #[test]
    fn settings_files_may_start_with_a_byte_order_mark() {
        let settings = format!("\u{feff}{PORTKEY_SETTINGS}");
        let loaded = load(&fixture(Some(&settings), None)).unwrap();
        assert!(loaded.diagnostics().is_empty());
        assert_eq!(
            loaded.selected_connection(&no_environment).unwrap().id,
            "portkey"
        );
    }

    #[test]
    fn permission_mode_and_acknowledgement_follow_workspace_overrides() {
        assert_eq!(
            fixture_settings("{}").permission_mode(),
            PermissionMode::Auto
        );
        let settings =
            fixture_settings(r#"{"permission_mode":"Full Access","yolo_acknowledged":true}"#);
        assert_eq!(settings.permission_mode(), PermissionMode::Yolo);
        assert!(settings.yolo_acknowledged());
        let fixture = fixture(None, None);
        let json = format!(
            r#"{{"permission_mode":"ask","workspaces":{{{}:{{"permission_mode":"yolo"}}}}}}"#,
            serde_json::to_string(&fixture.workspace.to_string_lossy()).unwrap()
        );
        fs::write(fixture.paths.config.join(SETTINGS_FILE), json).unwrap();
        let settings = load(&fixture).unwrap();
        assert_eq!(settings.permission_mode(), PermissionMode::Yolo);
        assert!(!settings.yolo_acknowledged());
    }

    #[test]
    fn effort_and_fast_mode_follow_workspace_overrides() {
        let defaults = fixture_settings("{}");
        assert_eq!(defaults.reasoning_effort(), ReasoningEffort::Auto);
        assert!(!defaults.fast_mode());
        let saved =
            fixture_settings(r#"{"effort":"xHigh","fast_mode":true,"fast_mode_model_bound":true}"#);
        assert!(saved.diagnostics().is_empty());
        assert_eq!(
            saved.reasoning_effort(),
            ReasoningEffort::Named("xHigh".to_owned())
        );
        assert!(saved.fast_mode());
        assert_eq!(
            fixture_settings(r#"{"effort":"Adaptive"}"#).reasoning_effort(),
            ReasoningEffort::Auto
        );
        let fixture = fixture(None, None);
        let workspace = serde_json::to_string(&fixture.workspace.to_string_lossy()).unwrap();
        for (entry, effort, fast) in [
            (
                r#"{"effort":null,"fast_mode":false}"#,
                ReasoningEffort::Auto,
                false,
            ),
            (
                r#"{"effort":"low"}"#,
                ReasoningEffort::Named("low".to_owned()),
                true,
            ),
        ] {
            let json = format!(
                r#"{{"effort":"high","fast_mode":true,"workspaces":{{{workspace}:{entry}}}}}"#
            );
            fs::write(fixture.paths.config.join(SETTINGS_FILE), json).unwrap();
            let settings = load(&fixture).unwrap();
            assert_eq!(settings.reasoning_effort(), effort, "{entry}");
            assert_eq!(settings.fast_mode(), fast, "{entry}");
        }
    }

    #[test]
    fn context_switch_follows_workspace_global_then_project_layers() {
        assert!(fixture_settings("{}").context_enabled());
        assert!(!fixture_settings(r#"{"context":false}"#).context_enabled());
        let project_off = load(&fixture(None, Some(r#"{"context":false}"#))).unwrap();
        assert!(project_off.diagnostics().is_empty());
        assert!(!project_off.context_enabled());
        let profile_on = load(&fixture(
            Some(r#"{"context":true}"#),
            Some(r#"{"context":false}"#),
        ))
        .unwrap();
        assert!(profile_on.context_enabled());
        let fixture = fixture(Some("{}"), None);
        let workspace = serde_json::to_string(&fixture.workspace.to_string_lossy()).unwrap();
        let json =
            format!(r#"{{"context":true,"workspaces":{{{workspace}:{{"context":false}}}}}}"#);
        fs::write(fixture.paths.config.join(SETTINGS_FILE), json).unwrap();
        assert!(!load(&fixture).unwrap().context_enabled());
    }

    #[test]
    fn invalid_context_switches_discard_their_layer() {
        let profile = fixture_settings(r#"{"context":"no","max_agent_steps":4}"#);
        assert_eq!(
            profile.diagnostics()[0].to_string(),
            "config user: malformed_settings"
        );
        assert!(profile.profile_is_unusable());
        assert!(profile.context_enabled());
        let project = load(&fixture(
            None,
            Some(r#"{"max_agent_steps":12,"context":null}"#),
        ))
        .unwrap();
        assert_eq!(
            project.diagnostics()[0].to_string(),
            "config project: malformed_settings"
        );
        assert!(!project.profile_is_unusable());
        assert_eq!(project.max_agent_steps(&no_environment), 0);
        assert!(project.context_enabled());
    }

    #[test]
    fn context_limits_resolve_compiled_global_and_workspace_sources() {
        let fixture = fixture(Some("{}"), None);
        let workspace = serde_json::to_string(&fixture.workspace.to_string_lossy()).unwrap();
        let json = format!(
            r#"{{"context_limits":{{"skill_chunk_bytes":111,"mcp_description_bytes":"off","project_instruction_file_bytes":12}},"workspaces":{{{workspace}:{{"context_limits":{{"skill_chunk_bytes":222}}}}}}}}"#
        );
        fs::write(fixture.paths.config.join(SETTINGS_FILE), json).unwrap();
        let settings = load(&fixture).unwrap();
        assert!(settings.diagnostics().is_empty());
        let limits = settings.context_limits();
        let chunk = limits.get(ContextLimitName::SkillChunkBytes);
        assert_eq!(chunk.effective_bytes(), 222);
        assert_eq!(chunk.source, ContextLimitSource::WorkspaceSettings);
        let description = limits.get(ContextLimitName::McpDescriptionBytes);
        assert_eq!(description.effective_bytes(), EMERGENCY_CEILING_BYTES);
        assert_eq!(description.source, ContextLimitSource::GlobalSettings);
        let file = limits.get(ContextLimitName::ProjectInstructionFileBytes);
        assert_eq!(file.effective_bytes(), 12);
        assert_eq!(file.source.label(), "global settings");
        let total = limits.get(ContextLimitName::ProjectInstructionsTotalBytes);
        assert_eq!(total.effective_bytes(), 128 * 1024);
        assert_eq!(total.source.label(), "compiled default");
    }

    #[test]
    fn invalid_context_limits_are_diagnosed_without_blocking_the_profile() {
        for json in [
            r#"{"provider":"local","context_limits":{"unknown_limit":10},"permission_mode":"ask","providers":{"local":{"protocol":"openai-chat-completions","base_url":"http://localhost:1234/v1","auth":{"type":"none"}}}}"#,
            r#"{"provider":"local","context_limits":[],"permission_mode":"ask","providers":{"local":{"protocol":"openai-chat-completions","base_url":"http://localhost:1234/v1","auth":{"type":"none"}}}}"#,
            r#"{"provider":"local","context_limits":{"skill_chunk_bytes":-1},"permission_mode":"ask","providers":{"local":{"protocol":"openai-chat-completions","base_url":"http://localhost:1234/v1","auth":{"type":"none"}}}}"#,
        ] {
            let settings = fixture_settings(json);
            assert_eq!(
                settings.diagnostics()[0].to_string(),
                "config user: invalid_context_limits; context_limits keys must be documented limit names with a non-negative integer or \"off\" value",
                "{json}"
            );
            assert!(!settings.profile_is_unusable(), "{json}");
            assert_eq!(settings.permission_mode(), PermissionMode::Auto, "{json}");
            assert_eq!(
                settings.selected_provider(&no_environment),
                Ok(ProviderId::Configured("local".to_owned())),
                "{json}"
            );
            assert_eq!(
                settings.context_limits(),
                ContextLimits::default(),
                "{json}"
            );
        }
        let model_first = fixture_settings(r#"{"model":" bad","context_limits":[]}"#);
        assert_eq!(
            model_first.diagnostics()[0].cause,
            DiagnosticCause::InvalidModelId
        );
        let project = load(&fixture(
            None,
            Some(r#"{"context_limits":{"unknown_limit":1}}"#),
        ))
        .unwrap();
        assert_eq!(
            project.diagnostics()[0].to_string(),
            "config project: ignored_project_user_only_setting; key=context_limits"
        );
        assert_eq!(project.context_limits(), ContextLimits::default());
    }

    #[test]
    fn provider_registry_errors_fail_the_load() {
        let json = r#"{"providers":{"local":{"protocol":"responses"}}}"#;
        assert_eq!(
            load(&fixture(Some(json), None)),
            Err(SettingsError::Providers(
                ConfiguredProviderError::InvalidProtocol
            ))
        );
    }

    #[test]
    fn project_files_ignore_profile_keys_and_supply_step_limits() {
        let fixture = fixture(
            Some(PORTKEY_SETTINGS),
            Some(r#"{"provider":"evil","providers":{},"max_agent_steps":12}"#),
        );
        let settings = load(&fixture).unwrap();
        let rendered: Vec<String> = settings
            .diagnostics()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            rendered,
            [
                "config project: ignored_project_user_only_setting; key=provider",
                "config project: ignored_project_user_only_setting; key=providers",
            ]
        );
        assert!(!settings.profile_is_unusable());
        assert_eq!(settings.max_agent_steps(&no_environment), 12);
        let environment = |name: &str| (name == MAX_AGENT_STEPS_VARIABLE).then(|| " 3 ".to_owned());
        assert_eq!(settings.max_agent_steps(&environment), 3);
        assert_eq!(
            settings.selected_connection(&no_environment).unwrap().id,
            "portkey"
        );
    }

    #[test]
    fn provider_order_lists_trim_entries_and_reject_invalid_duplicate_or_empty_lists() {
        for valid in [
            " azure, anthropic ,bedrock",
            "vertexAnthropic,,claudeaws",
            "a,b,c,d,e,f,g,h",
        ] {
            assert!(is_valid_provider_order_list(valid), "{valid:?}");
        }
        for invalid in [
            "azure,Bad Slug",
            "azure,azure",
            "-azure",
            "a,b,c,d,e,f,g,h,i",
            " , ,",
            "",
        ] {
            assert!(!is_valid_provider_order_list(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn provider_settings_keep_independent_provider_models() {
        let settings = fixture_settings(
            r#"{"provider":"codex","model":"gateway/model","codex_model":"gpt-5.4-mini"}"#,
        );
        assert_eq!(settings.codex_selected(&no_environment), Ok(true));
        assert_eq!(
            settings.selected_codex_model(None, &no_environment),
            Ok("gpt-5.4-mini".to_owned())
        );
        let current = fixture_settings(
            r#"{"provider":"CODEX","model":"legacy/gateway","codex_model":"legacy-codex","models":{"gateway":"current/gateway","codex":"current-codex"}}"#,
        );
        assert_eq!(current.codex_selected(&no_environment), Ok(true));
        assert_eq!(
            current.selected_codex_model(None, &no_environment),
            Ok("current-codex".to_owned())
        );
    }

    #[test]
    fn codex_model_precedence_runs_from_flag_to_environment_to_workspace_to_global() {
        let fixture = fixture(None, None);
        let json = format!(
            r#"{{"model":"gateway/model","models":{{"codex":"global-codex"}},"workspaces":{{{}:{{"codex_model":"workspace-codex"}}}}}}"#,
            serde_json::to_string(&fixture.workspace.to_string_lossy()).unwrap()
        );
        fs::write(fixture.paths.config.join(SETTINGS_FILE), json).unwrap();
        let settings = load(&fixture).unwrap();
        let codex = |name: &str| match name {
            PROVIDER_VARIABLE => Some("codex".to_owned()),
            MODEL_VARIABLE => Some(" gpt-env ".to_owned()),
            _ => None,
        };
        assert_eq!(settings.codex_selected(&no_environment), Ok(false));
        assert_eq!(settings.codex_selected(&codex), Ok(true));
        assert_eq!(
            settings.selected_codex_model(Some("gpt-flag"), &codex),
            Ok("gpt-flag".to_owned())
        );
        assert_eq!(
            settings.selected_codex_model(None, &codex),
            Ok("gpt-env".to_owned())
        );
        assert_eq!(
            settings.selected_codex_model(None, &no_environment),
            Ok("workspace-codex".to_owned())
        );
        let global = fixture_settings(r#"{"provider":"codex","models":{"codex":"global-codex"}}"#);
        assert_eq!(
            global.selected_codex_model(None, &no_environment),
            Ok("global-codex".to_owned())
        );
        let unsaved = fixture_settings(r#"{"provider":"codex","model":"gateway/model"}"#);
        let error = unsaved
            .selected_codex_model(None, &no_environment)
            .unwrap_err();
        assert_eq!(error, SelectionError::CodexModelNotSelected);
        assert_eq!(error.code(), "CodexModelNotSelected");
        assert_eq!(
            error.to_string(),
            "no Codex model is selected; run `oh-fx provider codex` to choose one, or set a model for this run with --model or OH_FX_MODEL"
        );
    }

    #[test]
    fn missing_settings_default_to_the_gateway() {
        let settings = load(&fixture(None, None)).unwrap();
        assert_eq!(
            settings.selected_provider(&no_environment),
            Ok(ProviderId::Gateway)
        );
        assert!(settings.diagnostics().is_empty());
    }
}
