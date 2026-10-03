use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ofx_config::{DurableError, PrivateDir, parse_strict_json};
use reqwest::StatusCode;
use serde_json::{Map, Value};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::client::{BoundedFailure, bounded_get};

const CACHE_DIRECTORY: &str = "provider-versions";
const CODEX_CACHE_FILE: &str = "codex.json";
const GROK_CACHE_FILE: &str = "grok.json";

#[derive(Clone, Copy)]
enum ProviderVersion {
    Codex,
    Grok,
}

impl ProviderVersion {
    const fn cache_file(self) -> &'static str {
        match self {
            Self::Codex => CODEX_CACHE_FILE,
            Self::Grok => GROK_CACHE_FILE,
        }
    }
}
const MAX_CACHE_BYTES: usize = 256;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const REFRESH_INTERVAL_MS: i64 = 60_000;
const TRIMMED: [char; 4] = [' ', '\r', '\n', '\t'];

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

    pub(crate) async fn resolve_grok(
        &self,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<Version, VersionError> {
        self.resolve_at_for(cancel, deadline, now_ms(), ProviderVersion::Grok)
            .await
    }

    async fn resolve_at(
        &self,
        cancel: &CancellationToken,
        deadline: Instant,
        now_ms: i64,
    ) -> Result<Version, VersionError> {
        self.resolve_at_for(cancel, deadline, now_ms, ProviderVersion::Codex)
            .await
    }

    async fn resolve_at_for(
        &self,
        cancel: &CancellationToken,
        deadline: Instant,
        now_ms: i64,
        provider: ProviderVersion,
    ) -> Result<Version, VersionError> {
        if cancel.is_cancelled() {
            return Err(VersionError::Cancelled);
        }
        let cached = self
            .cache_directory
            .and_then(|directory| load_provider_cache(directory, provider));
        if let Some(fresh) = cached.as_ref().filter(|cached| cached.fresh(now_ms)) {
            return Ok(fresh.version.clone());
        }
        let version = match self.fetch(cancel, deadline, provider).await {
            Ok(version) => version,
            Err(VersionError::Cancelled) => return Err(VersionError::Cancelled),
            Err(VersionError::Unavailable) => cached.ok_or(VersionError::Unavailable)?.version,
        };
        if let Some(directory) = self.cache_directory {
            let _ = save_provider_cache(
                directory,
                &Cached {
                    version: version.clone(),
                    checked_at_ms: now_ms,
                },
                provider,
            );
        }
        Ok(version)
    }

    async fn fetch(
        &self,
        cancel: &CancellationToken,
        outer_deadline: Instant,
        provider: ProviderVersion,
    ) -> Result<Version, VersionError> {
        let deadline = (Instant::now() + LOOKUP_TIMEOUT).min(outer_deadline);
        let request = self.client.get(self.url);
        match bounded_get(request, MAX_RESPONSE_BYTES, deadline, cancel).await {
            Ok((StatusCode::OK, body)) => match provider {
                ProviderVersion::Codex => parse_codex_release(&body),
                ProviderVersion::Grok => parse_grok_release(&body),
            }
            .ok_or(VersionError::Unavailable),
            Ok(_) | Err(BoundedFailure::Failed | BoundedFailure::TooLarge) => {
                Err(VersionError::Unavailable)
            }
            Err(BoundedFailure::Cancelled) => Err(VersionError::Cancelled),
        }
    }
}

fn parse_grok_release(body: &[u8]) -> Option<Version> {
    Version::parse(std::str::from_utf8(body).ok()?)
}

fn parse_codex_release(body: &[u8]) -> Option<Version> {
    match parse_strict_json(body).ok()? {
        Value::Object(release) => Version::parse(release.get("version")?.as_str()?),
        _ => None,
    }
}

#[cfg(test)]
fn load_cache(directory: &Path) -> Option<Cached> {
    load_provider_cache(directory, ProviderVersion::Codex)
}

fn load_provider_cache(directory: &Path, provider: ProviderVersion) -> Option<Cached> {
    let directory = PrivateDir::open_existing(&directory.join(CACHE_DIRECTORY)).ok()??;
    let bytes = directory
        .read_private(provider.cache_file(), MAX_CACHE_BYTES)
        .ok()??;
    let Value::Object(record) = parse_strict_json(&bytes).ok()? else {
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

#[cfg(test)]
fn save_cache(directory: &Path, cached: &Cached) -> Result<(), DurableError> {
    save_provider_cache(directory, cached, ProviderVersion::Codex)
}

fn save_provider_cache(
    directory: &Path,
    cached: &Cached,
    provider: ProviderVersion,
) -> Result<(), DurableError> {
    PrivateDir::open_or_create(directory)?;
    let versions = PrivateDir::open_or_create(&directory.join(CACHE_DIRECTORY))?;
    let text = format!(
        "{{\"version\":\"{}\",\"checked_at_ms\":{}}}\n",
        cached.version.as_str(),
        cached.checked_at_ms
    );
    versions.replace(provider.cache_file(), text.as_bytes())
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
