use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use ofx_config::{AdvisoryLock, DurableError, PrivateDir, ProfilePaths};
use serde_json::Value;

use crate::command_provider::AddIntent;
use crate::mcp_contract::{
    InvalidServerConfig, McpAuthConfig, McpServerConfig, ProfileConfigWarning, TransportType,
};
use crate::project_config::{McpConfigError, ProfileParseResult, parse_profile_document};

pub const PROFILE_CONFIG_FILE_NAME: &str = "mcp.json";
const PROFILE_LOCK_FILE_NAME: &str = "mcp.lock";
const MAX_PROFILE_CONFIG_BYTES: u64 = 1024 * 1024;
const PROFILE_LOCK_DEADLINE: Duration = Duration::from_secs(2);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, thiserror::Error)]
pub enum ProfileStoreError {
    #[error(transparent)]
    Config(#[from] McpConfigError),
    #[error(transparent)]
    InvalidServer(#[from] InvalidServerConfig),
    #[error("McpConfigAmbiguousServerKey")]
    McpConfigAmbiguousServerKey,
    #[error("McpConfigPathInvalid")]
    McpConfigPathInvalid,
    #[error("StreamTooLong")]
    StreamTooLong,
    #[error("LockBusy")]
    LockBusy,
    #[error(transparent)]
    Durable(#[from] DurableError),
    #[error("{0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRemoveOutcome {
    pub removed: bool,
    pub warning: Option<ProfileConfigWarning>,
}

pub fn profile_config_path(paths: &ProfilePaths) -> PathBuf {
    paths.config.join(PROFILE_CONFIG_FILE_NAME)
}

pub fn load_profile_document(path: &Path) -> Result<ProfileParseResult, ProfileStoreError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ProfileParseResult::default());
        }
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_PROFILE_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PROFILE_CONFIG_BYTES {
        return Err(ProfileStoreError::StreamTooLong);
    }
    Ok(parse_profile_document(&bytes)?)
}

pub fn add_profile_server(
    path: &Path,
    intent: AddIntent,
) -> Result<Option<ProfileConfigWarning>, ProfileStoreError> {
    let next = config_from_intent(intent);
    let profile = ProfileFile::lock(path)?;
    let mut document = load_profile_document(path)?;
    if !document.mutation_allowed {
        return Err(ProfileStoreError::McpConfigAmbiguousServerKey);
    }
    match document
        .configs
        .iter_mut()
        .find(|existing| existing.name == next.name)
    {
        Some(existing) => *existing = next,
        None => document.configs.push(next),
    }
    profile.save(&document.configs)?;
    Ok(document.diagnostic)
}

pub fn remove_profile_server(
    path: &Path,
    name: &str,
) -> Result<ProfileRemoveOutcome, ProfileStoreError> {
    let profile = ProfileFile::lock(path)?;
    let mut document = load_profile_document(path)?;
    if !document.mutation_allowed {
        return Err(ProfileStoreError::McpConfigAmbiguousServerKey);
    }
    let Some(index) = document
        .configs
        .iter()
        .position(|config| config.name == name)
    else {
        return Ok(ProfileRemoveOutcome {
            removed: false,
            warning: document.diagnostic,
        });
    };
    document.configs.remove(index);
    profile.save(&document.configs)?;
    Ok(ProfileRemoveOutcome {
        removed: true,
        warning: document.diagnostic,
    })
}

fn config_from_intent(intent: AddIntent) -> McpServerConfig {
    match intent {
        AddIntent::Local {
            name,
            command,
            args,
        } => McpServerConfig::stdio(name, command, args),
        AddIntent::Http { name, url } => McpServerConfig {
            allow_stored_credentials: true,
            ..McpServerConfig::remote(name, TransportType::Http, url)
        },
    }
}

pub fn render_profile_config(configs: &[McpServerConfig]) -> Result<String, InvalidServerConfig> {
    let mut out = String::from("{\"mcp\":{");
    for (index, config) in configs.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_json_string(&mut out, &config.name);
        out.push_str(":{");
        render_server(&mut out, config)?;
        out.push('}');
    }
    out.push_str("}}");
    Ok(out)
}

fn render_server(out: &mut String, config: &McpServerConfig) -> Result<(), InvalidServerConfig> {
    match config.transport {
        TransportType::Stdio => {
            out.push_str("\"type\":\"local\",\"command\":[");
            push_json_string(out, config.stdio_command()?);
            for arg in &config.args {
                out.push(',');
                push_json_string(out, arg);
            }
            out.push(']');
        }
        TransportType::Sse | TransportType::Http => {
            out.push_str("\"type\":\"");
            out.push_str(config.transport.as_str());
            out.push_str("\",\"url\":");
            push_json_string(out, config.remote_url()?);
        }
    }
    out.push_str(if config.enabled {
        ",\"enabled\":true"
    } else {
        ",\"enabled\":false"
    });
    if config.required {
        out.push_str(",\"required\":true");
    }
    let _ = write!(
        out,
        ",\"startup_timeout_ms\":{},\"operation_timeout_ms\":{}",
        config.startup_timeout_ms, config.operation_timeout_ms
    );
    if config.transport == TransportType::Stdio {
        let _ = write!(out, ",\"restart_limit\":{}", config.restart_limit);
    }
    push_string_map(
        out,
        "headers",
        config
            .headers
            .iter()
            .map(|header| (header.name.as_str(), header.value.as_str())),
    );
    push_string_map(
        out,
        "header_env",
        config
            .header_env
            .iter()
            .map(|reference| (reference.name.as_str(), reference.env.as_str())),
    );
    if let Some(env_name) = &config.bearer_token_env {
        out.push_str(",\"bearer_token_env\":");
        push_json_string(out, env_name);
    }
    if let Some(auth) = &config.auth {
        render_auth(out, auth);
    }
    push_string_map(
        out,
        "environment",
        config
            .env
            .iter()
            .map(|entry| (entry.key.as_str(), entry.value.as_str())),
    );
    Ok(())
}

fn render_auth(out: &mut String, auth: &McpAuthConfig) {
    let mut members = Vec::new();
    let strings = [
        ("resource", &auth.resource),
        ("issuer", &auth.issuer),
        ("client_id", &auth.client_id),
        ("client_secret_env", &auth.client_secret_env),
        ("client_metadata_url", &auth.client_metadata_url),
    ];
    for (key, value) in strings {
        if let Some(value) = value {
            members.push(format!("\"{key}\":{}", json_string(value)));
        }
    }
    if auth.scopes_configured || !auth.scopes.is_empty() {
        let scopes: Vec<String> = auth.scopes.iter().map(|scope| json_string(scope)).collect();
        members.push(format!("\"scopes\":[{}]", scopes.join(",")));
    }
    if let Some(port) = auth.callback_port {
        members.push(format!("\"callback_port\":{port}"));
    }
    out.push_str(",\"oauth\":{");
    out.push_str(&members.join(","));
    out.push('}');
}

fn push_string_map<'a>(
    out: &mut String,
    key: &str,
    entries: impl ExactSizeIterator<Item = (&'a str, &'a str)>,
) {
    if entries.len() == 0 {
        return;
    }
    let members: Vec<String> = entries
        .map(|(name, value)| format!("{}:{}", json_string(name), json_string(value)))
        .collect();
    let _ = write!(out, ",\"{key}\":{{{}}}", members.join(","));
}

fn push_json_string(out: &mut String, value: &str) {
    out.push_str(&json_string(value));
}

fn json_string(value: &str) -> String {
    Value::from(value).to_string()
}

struct ProfileFile<'a> {
    directory: PrivateDir,
    name: &'a str,
    _lock: AdvisoryLock,
}

impl<'a> ProfileFile<'a> {
    fn lock(path: &'a Path) -> Result<Self, ProfileStoreError> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(ProfileStoreError::McpConfigPathInvalid)?;
        let parent = path
            .parent()
            .filter(|parent| parent.parent().is_some())
            .ok_or(ProfileStoreError::McpConfigPathInvalid)?;
        let directory = PrivateDir::open_or_create(parent)?;
        let deadline = Instant::now() + PROFILE_LOCK_DEADLINE;
        loop {
            if let Some(lock) = directory.try_lock(PROFILE_LOCK_FILE_NAME)? {
                return Ok(Self {
                    directory,
                    name,
                    _lock: lock,
                });
            }
            if Instant::now() >= deadline {
                return Err(ProfileStoreError::LockBusy);
            }
            thread::sleep(LOCK_POLL_INTERVAL);
        }
    }

    fn save(&self, configs: &[McpServerConfig]) -> Result<(), ProfileStoreError> {
        let json = render_profile_config(configs)?;
        Ok(self.directory.replace(self.name, json.as_bytes())?)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};

    use super::*;
    use crate::mcp_contract::{EnvVar, HttpHeader, HttpHeaderEnv};

    fn profile_path(root: &Path) -> PathBuf {
        root.join("home/.config/oh-fx")
            .join(PROFILE_CONFIG_FILE_NAME)
    }

    fn local(name: &str, command: &str, args: &[&str]) -> AddIntent {
        AddIntent::Local {
            name: name.to_owned(),
            command: command.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    #[test]
    fn canonical_write_format_orders_every_field() {
        let local = McpServerConfig {
            env: vec![EnvVar {
                key: "A".to_owned(),
                value: "B".to_owned(),
            }],
            ..McpServerConfig::stdio("local", "node", vec!["server.js".to_owned()])
        };
        let api = McpServerConfig {
            required: true,
            headers: vec![HttpHeader {
                name: "X-Workspace".to_owned(),
                value: "one".to_owned(),
            }],
            header_env: vec![HttpHeaderEnv {
                name: "X-Org".to_owned(),
                env: "ORG_ENV".to_owned(),
            }],
            bearer_token_env: Some("MCP_TOKEN".to_owned()),
            auth: Some(McpAuthConfig {
                resource: Some("https://api.example.com/mcp".to_owned()),
                client_id: Some("client".to_owned()),
                scopes: vec!["a".to_owned(), "b".to_owned()],
                scopes_configured: true,
                callback_port: Some(3118),
                ..McpAuthConfig::default()
            }),
            ..McpServerConfig::remote("api", TransportType::Http, "https://api.example.com/mcp")
        };
        assert_eq!(
            render_profile_config(&[local, api]).unwrap(),
            concat!(
                "{\"mcp\":{\"local\":{\"type\":\"local\",\"command\":[\"node\",\"server.js\"],",
                "\"enabled\":true,\"startup_timeout_ms\":30000,\"operation_timeout_ms\":60000,",
                "\"restart_limit\":1,\"environment\":{\"A\":\"B\"}},",
                "\"api\":{\"type\":\"http\",\"url\":\"https://api.example.com/mcp\",\"enabled\":true,",
                "\"required\":true,\"startup_timeout_ms\":30000,\"operation_timeout_ms\":60000,",
                "\"headers\":{\"X-Workspace\":\"one\"},\"header_env\":{\"X-Org\":\"ORG_ENV\"},",
                "\"bearer_token_env\":\"MCP_TOKEN\",\"oauth\":{\"resource\":\"https://api.example.com/mcp\",",
                "\"client_id\":\"client\",\"scopes\":[\"a\",\"b\"],\"callback_port\":3118}}}}"
            )
        );
    }

    #[test]
    fn save_and_reload_preserves_mixed_stdio_and_sse_configs() {
        let configs = vec![
            McpServerConfig::stdio("local", "node", vec!["a b".to_owned()]),
            McpServerConfig {
                enabled: false,
                ..McpServerConfig::remote("events", TransportType::Sse, "https://e.test/sse")
            },
        ];
        let rendered = render_profile_config(&configs).unwrap();
        let reloaded = parse_profile_document(rendered.as_bytes()).unwrap();
        let mut expected = configs;
        expected[1].allow_stored_credentials = true;
        assert_eq!(reloaded.configs, expected);
    }

    #[test]
    fn stdio_lifecycle_policy_parses_defaults_and_roundtrips_explicit_values() {
        let parsed = parse_profile_document(
            br#"{"mcp":{"bounded":{"type":"stdio","command":["node"],"startup_timeout_ms":125,"operation_timeout_ms":250,"restart_limit":2}}}"#,
        )
        .unwrap();
        let config = &parsed.configs[0];
        assert_eq!(config.startup_timeout_ms, 125);
        assert_eq!(config.operation_timeout_ms, 250);
        assert_eq!(config.restart_limit, 2);
        let rendered = render_profile_config(&parsed.configs).unwrap();
        assert!(rendered.contains("\"restart_limit\":2"));
        assert_eq!(
            parse_profile_document(
                br#"{"mcp":{"bad":{"type":"local","command":["node"],"restart_limit":256}}}"#
            ),
            Err(McpConfigError::McpConfigInvalidRestartLimit)
        );
    }

    #[test]
    fn add_profile_server_to_path_roundtrips_local_replacement_and_remove() {
        let temp = tempfile::tempdir().unwrap();
        let path = profile_path(temp.path());
        add_profile_server(&path, local("one", "node", &["a.js"])).unwrap();
        add_profile_server(&path, local("two", "python", &[])).unwrap();
        add_profile_server(&path, local("one", "deno", &["b.ts"])).unwrap();
        let document = load_profile_document(&path).unwrap();
        let names: Vec<_> = document
            .configs
            .iter()
            .map(|config| config.name.as_str())
            .collect();
        assert_eq!(names, ["one", "two"]);
        assert_eq!(document.configs[0].command.as_deref(), Some("deno"));
        let removed = remove_profile_server(&path, "one").unwrap();
        assert!(removed.removed);
        assert!(!remove_profile_server(&path, "missing").unwrap().removed);
        assert_eq!(load_profile_document(&path).unwrap().configs.len(), 1);
        let directory_mode = fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn adding_an_mcp_server_creates_the_profile_directory_privately_and_canonically() {
        let temp = tempfile::tempdir().unwrap();
        let path = profile_path(temp.path());
        add_profile_server(
            &path,
            AddIntent::Http {
                name: "docs".to_owned(),
                url: "https://example.test/mcp".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"mcp\":{\"docs\":{\"type\":\"http\",\"url\":\"https://example.test/mcp\",\"enabled\":true,\"startup_timeout_ms\":30000,\"operation_timeout_ms\":60000}}}"
        );
    }

    #[test]
    fn profile_mutation_rewrites_alias_files_canonically() {
        let temp = tempfile::tempdir().unwrap();
        let path = profile_path(temp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"mcpServers":{"old":{"command":"node"}}}"#).unwrap();
        add_profile_server(&path, local("new", "deno", &[])).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\"mcp\":{\"old\":"));
        assert!(!text.contains("mcpServers"));
    }

    #[test]
    fn profile_mutation_refuses_files_with_suspicious_sibling_maps() {
        let temp = tempfile::tempdir().unwrap();
        let path = profile_path(temp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = r#"{"mcp":{},"servers":{"x":{"command":"node"}}}"#;
        fs::write(&path, original).unwrap();
        assert!(matches!(
            add_profile_server(&path, local("new", "deno", &[])),
            Err(ProfileStoreError::McpConfigAmbiguousServerKey)
        ));
        assert!(matches!(
            remove_profile_server(&path, "x"),
            Err(ProfileStoreError::McpConfigAmbiguousServerKey)
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn saving_mcp_config_refuses_a_symlinked_target() {
        let temp = tempfile::tempdir().unwrap();
        let path = profile_path(temp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let elsewhere = temp.path().join("elsewhere.json");
        fs::write(&elsewhere, "{\"mcp\":{}}").unwrap();
        symlink(&elsewhere, &path).unwrap();
        assert!(matches!(
            add_profile_server(&path, local("one", "node", &[])),
            Err(ProfileStoreError::Durable(DurableError::PathUnsafe))
        ));
        assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "{\"mcp\":{}}");
    }

    #[test]
    fn remove_server_from_path_remains_permissive_for_odd_legacy_names() {
        let temp = tempfile::tempdir().unwrap();
        let path = profile_path(temp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"mcp":{"odd name/1":{"command":"node"}}}"#).unwrap();
        assert!(remove_profile_server(&path, "odd name/1").unwrap().removed);
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"mcp\":{}}");
    }

    #[test]
    fn loading_a_missing_profile_yields_no_servers_and_oversized_files_fail() {
        let temp = tempfile::tempdir().unwrap();
        let path = profile_path(temp.path());
        assert_eq!(
            load_profile_document(&path).unwrap(),
            ProfileParseResult::default()
        );
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
        assert!(matches!(
            load_profile_document(&path),
            Err(ProfileStoreError::StreamTooLong)
        ));
    }

    #[test]
    fn profile_config_path_lives_in_the_xdg_config_directory() {
        let paths = ProfilePaths {
            config: PathBuf::from("/home/ada/.config/oh-fx"),
            data: PathBuf::from("/data"),
            state: PathBuf::from("/state"),
            cache: PathBuf::from("/cache"),
        };
        assert_eq!(
            profile_config_path(&paths),
            Path::new("/home/ada/.config/oh-fx/mcp.json")
        );
    }
}
