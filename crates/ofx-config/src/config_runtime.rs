use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

use ofx_contract::PermissionMode;
use serde_json::{Map, Value};

use crate::configured_provider::{
    ConfiguredProviderError, ProviderDefinition, ProviderRegistry, validate_model_id,
};
use crate::model_provider::ProviderId;
use crate::paths::ProfilePaths;
use crate::strict_json;

const SETTINGS_FILE: &str = "settings.json";
const PROJECT_FILE: &str = ".oh-fx.json";
const MAX_SETTINGS_BYTES: u64 = 64 * 1024;
const MAX_MODEL_PREFERENCES: usize = 35;
const PROVIDER_VARIABLE: &str = "OH_FX_PROVIDER";
const MODEL_VARIABLE: &str = "OH_FX_MODEL";
const MAX_AGENT_STEPS_VARIABLE: &str = "OH_FX_MAX_AGENT_STEPS";
const BYTE_ORDER_MARK: &[u8] = b"\xef\xbb\xbf";
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
}

impl DiagnosticCause {
    const fn label(self) -> &'static str {
        match self {
            Self::MalformedSettings => "malformed_settings",
            Self::SettingsTooLarge => "settings_too_large",
            Self::DurablePathUnsafe => "durable_path_unsafe",
            Self::InvalidModelId => "invalid_model_id",
            Self::IgnoredProjectUserOnlySetting => "ignored_project_user_only_setting",
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
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectionError {
    #[error("InvalidProviderValue")]
    InvalidProviderValue,
    #[error("UnknownConfiguredProvider")]
    UnknownConfiguredProvider,
    #[error(
        "the {0} provider is not available in oh-fx yet; add a connection under \"providers\" in ~/.config/oh-fx/settings.json and select it with \"provider\" or OH_FX_PROVIDER"
    )]
    ProviderUnavailable(String),
    #[error("{MODEL_NOT_SELECTED}")]
    ModelNotSelected,
}

impl SelectionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidProviderValue => "InvalidProviderValue",
            Self::UnknownConfiguredProvider => "UnknownConfiguredProvider",
            Self::ProviderUnavailable(_) => "ProviderUnavailable",
            Self::ModelNotSelected => "ConfiguredModelNotSelected",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Layer {
    provider: Option<String>,
    model: Option<String>,
    models: Vec<(ProviderId, String)>,
    permission_mode: Option<PermissionMode>,
    yolo_acknowledged: Option<bool>,
    max_agent_steps: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    providers: ProviderRegistry,
    global: Layer,
    workspace: Layer,
    project_max_agent_steps: Option<u64>,
    diagnostics: Vec<ConfigDiagnostic>,
}

impl From<LayerError> for DiagnosticCause {
    fn from(error: LayerError) -> Self {
        match error {
            LayerError::InvalidModelValue => Self::InvalidModelId,
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

    pub(crate) fn selected_provider(
        &self,
        lookup: EnvironmentLookup<'_>,
    ) -> Result<ProviderId, SelectionError> {
        let from_environment = lookup(PROVIDER_VARIABLE).filter(|value| !value.trim().is_empty());
        let raw = from_environment
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
            .or_else(|| {
                lookup(MODEL_VARIABLE)
                    .map(|model| model.trim().to_owned())
                    .filter(|model| !model.is_empty())
            })
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
        match parse_steps(&project) {
            Ok(steps) => self.project_max_agent_steps = steps,
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
    file.take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| DiagnosticCause::DurablePathUnsafe)?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(DiagnosticCause::SettingsTooLarge);
    }
    Ok(Some(bytes))
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
    layer.yolo_acknowledged = object
        .get("yolo_acknowledged")
        .map(|value| {
            value
                .as_bool()
                .ok_or(LayerError::InvalidYoloAcknowledgedType)
        })
        .transpose()?;
    layer.max_agent_steps = parse_steps(object)?;
    Ok(layer)
}

fn parse_routing(object: &Map<String, Value>) -> Result<Layer, LayerError> {
    let provider = match object.get("provider") {
        None => None,
        Some(Value::String(provider)) => Some(provider.clone()),
        Some(_) => return Err(LayerError::InvalidProviderType),
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

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
    fn broken_profiles_are_diagnosed_and_block_runtime_use() {
        let malformed = load(&fixture(Some("{not json"), None)).unwrap();
        assert!(malformed.profile_is_unusable());
        assert_eq!(
            malformed.diagnostics()[0].to_string(),
            "config user: malformed_settings"
        );
        let duplicate = load(&fixture(Some(r#"{"provider":"a","provider":"b"}"#), None)).unwrap();
        assert!(duplicate.profile_is_unusable());
        let large = "x".repeat(usize::try_from(MAX_SETTINGS_BYTES).unwrap() + 1);
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
    fn missing_settings_default_to_the_gateway() {
        let settings = load(&fixture(None, None)).unwrap();
        assert_eq!(
            settings.selected_provider(&no_environment),
            Ok(ProviderId::Gateway)
        );
        assert!(settings.diagnostics().is_empty());
    }
}
