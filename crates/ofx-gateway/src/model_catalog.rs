use std::fmt;

use ofx_trace::{TraceContext, trace_event};
use zeroize::Zeroizing;

const CATALOG: &str = "catalog";
const HOST_MANAGED: &str = "host_managed";
const FX_LOGIN: &str = "fx_login";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogFailure {
    Authentication,
    RateLimited,
    GatewayUnavailable { retryable: bool },
    Cancellation,
    Transport,
    MalformedResponse,
    HttpStatus,
}

impl CatalogFailure {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::RateLimited => "rate_limited",
            Self::GatewayUnavailable { .. } => "gateway_unavailable",
            Self::Cancellation => "cancellation",
            Self::Transport => "transport",
            Self::MalformedResponse => "malformed_response",
            Self::HttpStatus => "http_status",
        }
    }

    pub const fn error_name(self) -> &'static str {
        match self {
            Self::Authentication => "AuthenticationRejected",
            Self::RateLimited => "RateLimited",
            Self::GatewayUnavailable { .. } => "GatewayUnavailable",
            Self::Cancellation => "Cancelled",
            Self::Transport => "TransportFailure",
            Self::MalformedResponse => "MalformedResponse",
            Self::HttpStatus => "Unavailable",
        }
    }

    pub const fn retryable(self) -> bool {
        match self {
            Self::RateLimited | Self::Transport => true,
            Self::GatewayUnavailable { retryable } => retryable,
            Self::Authentication
            | Self::Cancellation
            | Self::MalformedResponse
            | Self::HttpStatus => false,
        }
    }
}

pub(crate) fn failure_for_http_status(status: u16) -> CatalogFailure {
    match status {
        401 | 403 => CatalogFailure::Authentication,
        429 => CatalogFailure::RateLimited,
        500..=599 => CatalogFailure::GatewayUnavailable {
            retryable: matches!(status, 500 | 502..=504),
        },
        _ => CatalogFailure::HttpStatus,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogFailed {
    pub failure: CatalogFailure,
    pub http_status: Option<u16>,
    pub retryable: bool,
}

impl CatalogFailed {
    pub(crate) const fn without_status(failure: CatalogFailure, retryable: bool) -> Self {
        Self {
            failure,
            http_status: None,
            retryable,
        }
    }

    pub(crate) fn for_http_status(status: u16) -> Self {
        let failure = failure_for_http_status(status);
        Self {
            failure,
            http_status: Some(status),
            retryable: failure.retryable(),
        }
    }

    const fn allows_public_fallback(self) -> bool {
        matches!(self.failure, CatalogFailure::Authentication)
            && matches!(self.http_status, Some(401 | 403))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicOnlyReason {
    NoCredential,
    LoginTeamRequired,
    LoginRefreshRequired,
    CredentialRejected(&'static str),
}

impl PublicOnlyReason {
    const fn label(self) -> &'static str {
        match self {
            Self::NoCredential => "no_credential",
            Self::LoginTeamRequired => "fx_login_team_required",
            Self::LoginRefreshRequired => "fx_login_refresh_required",
            Self::CredentialRejected(_) => "authenticated_credential_rejected",
        }
    }

    const fn source(self) -> Option<&'static str> {
        match self {
            Self::NoCredential => None,
            Self::LoginTeamRequired | Self::LoginRefreshRequired => Some(FX_LOGIN),
            Self::CredentialRejected(source) => Some(source),
        }
    }
}

pub enum CatalogAccess {
    PublicOnly(PublicOnlyReason),
    Authenticated {
        source: &'static str,
        credential: Zeroizing<String>,
        login_team: Option<String>,
        explicit: bool,
    },
    HostManaged,
}

impl CatalogAccess {
    pub(crate) fn credential(&self) -> Option<&str> {
        match self {
            Self::Authenticated { credential, .. } => Some(credential.as_str()),
            Self::PublicOnly(_) | Self::HostManaged => None,
        }
    }

    pub(crate) fn login_team(&self) -> Option<&str> {
        match self {
            Self::Authenticated { login_team, .. } => login_team.as_deref(),
            Self::PublicOnly(_) | Self::HostManaged => None,
        }
    }

    pub(crate) fn public_fallback_after(&self, failed: CatalogFailed) -> Option<Self> {
        match self {
            Self::Authenticated {
                source,
                explicit: false,
                ..
            } if failed.allows_public_fallback() => Some(Self::PublicOnly(
                PublicOnlyReason::CredentialRejected(source),
            )),
            _ => None,
        }
    }
}

impl fmt::Debug for CatalogAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PublicOnly(reason) => formatter.debug_tuple("PublicOnly").field(reason).finish(),
            Self::Authenticated {
                source,
                login_team,
                explicit,
                ..
            } => formatter
                .debug_struct("Authenticated")
                .field("source", source)
                .field("credential", &"<redacted>")
                .field("login_team", login_team)
                .field("explicit", explicit)
                .finish(),
            Self::HostManaged => formatter.write_str("HostManaged"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessMetadata {
    pub source: Option<&'static str>,
    pub public_only: Option<PublicOnlyReason>,
}

impl AccessMetadata {
    pub(crate) fn of(access: &CatalogAccess) -> Self {
        match access {
            CatalogAccess::PublicOnly(reason) => Self {
                source: reason.source(),
                public_only: Some(*reason),
            },
            CatalogAccess::Authenticated { source, .. } => Self {
                source: Some(source),
                public_only: None,
            },
            CatalogAccess::HostManaged => Self {
                source: Some(HOST_MANAGED),
                public_only: None,
            },
        }
    }

    const fn level(self) -> &'static str {
        if self.public_only.is_some() {
            "public_only"
        } else {
            "authenticated"
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogFetch<T> {
    Loaded {
        models: Vec<T>,
        access: AccessMetadata,
        anonymous_fallback: bool,
        fallback_failure: Option<CatalogFailed>,
    },
    Failed {
        access: AccessMetadata,
        anonymous_fallback: bool,
        failure: CatalogFailed,
    },
}

pub(crate) fn trace_load<T>(requested: AccessMetadata, outcome: &CatalogFetch<T>) {
    trace_event!(
        CATALOG,
        "model_catalog_load",
        TraceContext::default(),
        "{}",
        load_line(requested, outcome)
    );
}

fn load_line<T>(requested: AccessMetadata, outcome: &CatalogFetch<T>) -> String {
    let (effective, anonymous_fallback, label, failure) = match outcome {
        CatalogFetch::Loaded {
            access,
            anonymous_fallback,
            fallback_failure,
            ..
        } => (access, anonymous_fallback, "loaded", *fallback_failure),
        CatalogFetch::Failed {
            access,
            anonymous_fallback,
            failure,
        } => (access, anonymous_fallback, "failed", Some(*failure)),
    };
    let status = failure
        .and_then(|failed| failed.http_status)
        .map_or_else(|| "none".to_owned(), |status| status.to_string());
    let retryable = failure.map_or(
        "none",
        |failed| {
            if failed.retryable { "true" } else { "false" }
        },
    );
    format!(
        "requested_access={} credential_source={} effective_access={} public_only_reason={} anonymous_fallback={anonymous_fallback} outcome={label} failure_category={} http_status={status} retryable={retryable}",
        requested.level(),
        requested.source.unwrap_or("none"),
        effective.level(),
        effective
            .public_only
            .map_or("none", PublicOnlyReason::label),
        failure.map_or("none", |failed| failed.failure.label()),
    )
}

#[cfg(test)]
mod tests;
