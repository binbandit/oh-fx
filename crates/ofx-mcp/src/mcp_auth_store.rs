use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use ofx_config::{AdvisoryLock, DurableError, PrivateDir};
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::error::McpError;
use crate::mcp_auth::{Credentials, issuers_match, parse_json, resource_covers_endpoint};
use crate::mcp_contract::McpServerConfig;
use crate::oauth_uri::canonical_resource;

const SCHEMA_VERSION: i64 = 1;
const MAX_STORE_BYTES: usize = 1024 * 1024;
const DIRECTORY_NAME: &str = "mcp-credentials";
const FILE_NAME: &str = "credentials.json";
const LOCK_FILE_NAME: &str = "credentials.lock";
const LOCK_DEADLINE: Duration = Duration::from_secs(2);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CredentialStore {
    data: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SaveResult {
    pub(crate) repaired_entries: usize,
}

#[derive(Debug, Default)]
struct Store {
    credentials: Vec<(String, Credentials)>,
    rejected_entries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrantLookup {
    identity: String,
    endpoint: String,
    resource: Option<String>,
    issuer: Option<String>,
}

impl GrantLookup {
    pub(crate) fn new(
        identity: &str,
        endpoint: &str,
        resource: Option<&str>,
        issuer: Option<&str>,
    ) -> Result<Self, McpError> {
        Ok(Self {
            identity: identity.to_owned(),
            endpoint: canonical_resource(endpoint)?,
            resource: resource.map(canonical_resource).transpose()?,
            issuer: issuer.map(str::to_owned),
        })
    }

    pub(crate) fn for_server(config: &McpServerConfig) -> Result<Self, McpError> {
        let auth = config.auth.as_ref();
        Self::new(
            &config.name,
            config.remote_url()?,
            auth.and_then(|auth| auth.resource.as_deref()),
            auth.and_then(|auth| auth.issuer.as_deref()),
        )
    }

    fn matches(&self, identity: &str, credentials: &Credentials) -> bool {
        identity == self.identity
            && credentials.endpoint == self.endpoint
            && self
                .resource
                .as_deref()
                .is_none_or(|resource| resource_covers_endpoint(&credentials.resource, resource))
            && self
                .issuer
                .as_deref()
                .is_none_or(|issuer| issuers_match(&credentials.issuer, issuer))
    }
}

struct LockedDir {
    directory: PrivateDir,
    _lock: AdvisoryLock,
}

impl CredentialStore {
    pub(crate) fn new(data: &Path) -> Self {
        Self {
            data: data.to_path_buf(),
        }
    }

    pub(crate) fn load(&self, lookup: &GrantLookup) -> Result<Option<Credentials>, McpError> {
        let Some(locked) = self.open_existing()? else {
            return Ok(None);
        };
        let store = load_store(&locked.directory)?;
        let mut matched = None;
        for (identity, credentials) in &store.credentials {
            if !lookup.matches(identity, credentials) {
                continue;
            }
            if matched.is_some() {
                return Ok(None);
            }
            matched = Some(credentials);
        }
        Ok(matched.cloned())
    }

    pub(crate) fn save(
        &self,
        lookup: &GrantLookup,
        credentials: &Credentials,
    ) -> Result<SaveResult, McpError> {
        let locked = self.open_or_create()?;
        let mut store = load_store(&locked.directory)?;
        let superseded = |identity: &str, entry: &Credentials| {
            lookup.matches(identity, entry)
                || (identity == lookup.identity
                    && entry.endpoint == credentials.endpoint
                    && entry.resource == credentials.resource
                    && entry.issuer == credentials.issuer)
        };
        let mut slot = None;
        let mut kept = Vec::with_capacity(store.credentials.len() + 1);
        for (identity, entry) in store.credentials.drain(..) {
            if superseded(&identity, &entry) {
                slot.get_or_insert(kept.len());
            } else {
                kept.push((identity, entry));
            }
        }
        let replacement = (lookup.identity.clone(), credentials.clone());
        match slot {
            Some(index) => kept.insert(index, replacement),
            None => kept.push(replacement),
        }
        store.credentials = kept;
        let bytes = serialize_store(&store)?;
        locked.directory.replace(FILE_NAME, bytes.as_bytes())?;
        Ok(SaveResult {
            repaired_entries: store.rejected_entries,
        })
    }

    fn open_existing(&self) -> Result<Option<LockedDir>, McpError> {
        let Some(root) = PrivateDir::open_existing_private(&self.data)? else {
            return Ok(None);
        };
        let Some(directory) = root.open_child_private(DIRECTORY_NAME)? else {
            return Ok(None);
        };
        lock(directory).map(Some)
    }

    fn open_or_create(&self) -> Result<LockedDir, McpError> {
        let root = PrivateDir::open_or_create(&self.data)?;
        lock(root.open_or_create_child(DIRECTORY_NAME)?)
    }
}

fn lock(directory: PrivateDir) -> Result<LockedDir, McpError> {
    let deadline = Instant::now() + LOCK_DEADLINE;
    loop {
        if let Some(lock) = directory.try_lock(LOCK_FILE_NAME)? {
            return Ok(LockedDir {
                directory,
                _lock: lock,
            });
        }
        if Instant::now() >= deadline {
            return Err(McpError::LockBusy);
        }
        thread::sleep(LOCK_POLL_INTERVAL);
    }
}

fn load_store(directory: &PrivateDir) -> Result<Store, McpError> {
    match directory.read_exact_private(FILE_NAME, MAX_STORE_BYTES) {
        Ok(Some(bytes)) => parse_store(&bytes),
        Ok(None) => Ok(Store::default()),
        Err(DurableError::TooLarge) => Err(McpError::StreamTooLong),
        Err(error) => Err(error.into()),
    }
}

fn parse_store(bytes: &[u8]) -> Result<Store, McpError> {
    let Value::Object(document) = parse_json(bytes)? else {
        return Err(McpError::InvalidMcpCredentialStore);
    };
    if document.get("version").and_then(Value::as_i64) != Some(SCHEMA_VERSION) {
        return Err(McpError::InvalidMcpCredentialStore);
    }
    let Some(Value::Array(values)) = document.get("credentials") else {
        return Err(McpError::InvalidMcpCredentialStore);
    };
    let mut store = Store::default();
    for value in values {
        let parsed = value.as_object().ok_or(Rejected).and_then(|object| {
            Ok((
                required_string(object, "server_identity")?,
                parse_credentials(object)?,
            ))
        });
        match parsed {
            Ok(entry) => store.credentials.push(entry),
            Err(Rejected) => store.rejected_entries += 1,
        }
    }
    Ok(store)
}

struct Rejected;

fn parse_credentials(object: &Map<String, Value>) -> Result<Credentials, Rejected> {
    let expires_at_ms = match object.get("expires_at_ms") {
        Some(Value::Null) => i64::MAX,
        Some(Value::Number(number)) => number.as_i64().ok_or(Rejected)?,
        _ => return Err(Rejected),
    };
    Ok(Credentials {
        endpoint: required_string(object, "endpoint")?,
        resource: required_string(object, "resource")?,
        issuer: required_string(object, "issuer")?,
        client_id: required_string(object, "client_id")?,
        client_secret: optional_string(object, "client_secret")?.map(Zeroizing::new),
        access_token: Zeroizing::new(required_string(object, "access_token")?),
        refresh_token: optional_string(object, "refresh_token")?.map(Zeroizing::new),
        scope: object
            .get("scope")
            .and_then(Value::as_str)
            .ok_or(Rejected)?
            .to_owned(),
        token_type: required_string(object, "token_type")?,
        token_endpoint_auth_method: required_string(object, "token_endpoint_auth_method")?,
        expires_at_ms,
        authorization_endpoint: required_string(object, "authorization_endpoint")?,
        token_endpoint: required_string(object, "token_endpoint")?,
        revocation_endpoint: optional_string(object, "revocation_endpoint")?,
    })
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, Rejected> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(Rejected)
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, Rejected> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(_) => Err(Rejected),
    }
}

fn serialize_store(store: &Store) -> Result<Zeroizing<String>, McpError> {
    let mut out = Zeroizing::new(String::from("{\"version\":1,\"credentials\":["));
    for (index, (identity, credentials)) in store.credentials.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_entry(&mut out, identity, credentials);
    }
    out.push_str("]}");
    if out.len() > MAX_STORE_BYTES {
        return Err(McpError::McpCredentialStoreTooLarge);
    }
    Ok(out)
}

fn write_entry(out: &mut String, identity: &str, credentials: &Credentials) {
    out.push_str("{\"server_identity\":");
    push_string(out, identity);
    for (key, value) in [
        ("endpoint", credentials.endpoint.as_str()),
        ("resource", &credentials.resource),
        ("issuer", &credentials.issuer),
        ("client_id", &credentials.client_id),
    ] {
        let _ = write!(out, ",\"{key}\":");
        push_string(out, value);
    }
    out.push_str(",\"client_secret\":");
    push_optional(
        out,
        credentials.client_secret.as_deref().map(String::as_str),
    );
    out.push_str(",\"access_token\":");
    push_string(out, &credentials.access_token);
    out.push_str(",\"refresh_token\":");
    push_optional(
        out,
        credentials.refresh_token.as_deref().map(String::as_str),
    );
    for (key, value) in [
        ("scope", credentials.scope.as_str()),
        ("token_type", &credentials.token_type),
        (
            "token_endpoint_auth_method",
            &credentials.token_endpoint_auth_method,
        ),
    ] {
        let _ = write!(out, ",\"{key}\":");
        push_string(out, value);
    }
    let _ = write!(out, ",\"expires_at_ms\":{}", credentials.expires_at_ms);
    out.push_str(",\"authorization_endpoint\":");
    push_string(out, &credentials.authorization_endpoint);
    out.push_str(",\"token_endpoint\":");
    push_string(out, &credentials.token_endpoint);
    out.push_str(",\"revocation_endpoint\":");
    push_optional(out, credentials.revocation_endpoint.as_deref());
    out.push('}');
}

fn push_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            control if u32::from(control) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

fn push_optional(out: &mut String, value: Option<&str>) {
    match value {
        Some(value) => push_string(out, value),
        None => out.push_str("null"),
    }
}

#[cfg(test)]
mod tests;
