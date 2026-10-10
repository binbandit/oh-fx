use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ofx_config::{DurableError, PrivateDir};
use ofx_contract::parse_strict_json_value;
use ofx_trace::trace_log;
use reqwest::StatusCode;
use serde_json::{Map, Value};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::client::{BoundedFailure, bounded_get};

const CACHE_DIRECTORY: &str = "provider-versions";
const CODEX_CACHE_FILE: &str = "codex.json";
const MAX_CACHE_BYTES: usize = 256;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const REFRESH_INTERVAL_MS: i64 = 60_000;
const TRIMMED: [char; 4] = [' ', '\r', '\n', '\t'];
const MODELS: &str = "models";
const PROVIDER: &str = "codex";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Version(String);

impl Version {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let value = ofx_upgrade::normalize_version(raw.trim_matches(TRIMMED));
        ofx_upgrade::is_valid_version(value).then(|| Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VersionError {
    Cancelled,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cached {
    version: Version,
    checked_at_ms: i64,
}

impl Cached {
    fn fresh(&self, now_ms: i64) -> bool {
        now_ms
            .checked_sub(self.checked_at_ms)
            .is_some_and(|age| (0..REFRESH_INTERVAL_MS).contains(&age))
    }
}

pub(crate) struct VersionLookup<'a> {
    pub(crate) client: &'a reqwest::Client,
    pub(crate) url: &'a str,
    pub(crate) cache_directory: Option<&'a Path>,
}

impl VersionLookup<'_> {
    pub(crate) async fn resolve(
        &self,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<Version, VersionError> {
        self.resolve_at(cancel, deadline, now_ms()).await
    }

    async fn resolve_at(
        &self,
        cancel: &CancellationToken,
        deadline: Instant,
        now_ms: i64,
    ) -> Result<Version, VersionError> {
        if cancel.is_cancelled() {
            return Err(VersionError::Cancelled);
        }
        let cached = self.cache_directory.and_then(load_cache);
        if let Some(fresh) = cached.as_ref().filter(|cached| cached.fresh(now_ms)) {
            return Ok(fresh.version.clone());
        }
        let version = match self.fetch(cancel, deadline).await {
            Ok(version) => version,
            Err(VersionError::Cancelled) => return Err(VersionError::Cancelled),
            Err(VersionError::Unavailable) => cached.ok_or(VersionError::Unavailable)?.version,
        };
        if let Some(directory) = self.cache_directory {
            let _ = save_cache(
                directory,
                &Cached {
                    version: version.clone(),
                    checked_at_ms: now_ms,
                },
            );
        }
        Ok(version)
    }

    async fn fetch(
        &self,
        cancel: &CancellationToken,
        outer_deadline: Instant,
    ) -> Result<Version, VersionError> {
        let deadline = (Instant::now() + LOOKUP_TIMEOUT).min(outer_deadline);
        let request = self.client.get(self.url);
        match bounded_get(request, MAX_RESPONSE_BYTES, deadline, cancel).await {
            Ok((StatusCode::OK, body)) => parse_codex_release(&body).ok_or_else(|| {
                trace_log!(
                    MODELS,
                    "provider version metadata invalid provider={PROVIDER}"
                );
                VersionError::Unavailable
            }),
            Ok((status, _)) => {
                trace_log!(
                    MODELS,
                    "provider version lookup rejected provider={PROVIDER} status={}",
                    status.as_u16()
                );
                Err(VersionError::Unavailable)
            }
            Err(BoundedFailure::Failed(code)) => {
                trace_log!(
                    MODELS,
                    "provider version lookup failed provider={PROVIDER} err={code}"
                );
                Err(VersionError::Unavailable)
            }
            Err(BoundedFailure::Cancelled) => Err(VersionError::Cancelled),
        }
    }
}

fn parse_codex_release(body: &[u8]) -> Option<Version> {
    match parse_strict_json_value(body).ok()? {
        Value::Object(release) => Version::parse(release.get("version")?.as_str()?),
        _ => None,
    }
}

fn load_cache(directory: &Path) -> Option<Cached> {
    let directory = PrivateDir::open_existing(&directory.join(CACHE_DIRECTORY)).ok()??;
    let bytes = directory
        .read_private(CODEX_CACHE_FILE, MAX_CACHE_BYTES)
        .ok()??;
    let Value::Object(record) = parse_strict_json_value(&bytes).ok()? else {
        return None;
    };
    parse_cache_record(&record)
}

fn parse_cache_record(record: &Map<String, Value>) -> Option<Cached> {
    if record.len() != 2 {
        return None;
    }
    Some(Cached {
        version: Version::parse(record.get("version")?.as_str()?)?,
        checked_at_ms: record.get("checked_at_ms")?.as_i64()?,
    })
}

fn save_cache(directory: &Path, cached: &Cached) -> Result<(), DurableError> {
    PrivateDir::open_or_create(directory)?;
    let versions = PrivateDir::open_or_create(&directory.join(CACHE_DIRECTORY))?;
    let text = format!(
        "{{\"version\":\"{}\",\"checked_at_ms\":{}}}\n",
        cached.version.as_str(),
        cached.checked_at_ms
    );
    versions.replace(CODEX_CACHE_FILE, text.as_bytes())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
