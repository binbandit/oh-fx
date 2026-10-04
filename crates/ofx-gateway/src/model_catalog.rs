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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_http_failures_follow_the_upstream_categories() {
        let unavailable = |retryable| CatalogFailure::GatewayUnavailable { retryable };
        for (status, expected) in [
            (401, CatalogFailure::Authentication),
            (403, CatalogFailure::Authentication),
            (429, CatalogFailure::RateLimited),
            (500, unavailable(true)),
            (501, unavailable(false)),
            (502, unavailable(true)),
            (503, unavailable(true)),
            (504, unavailable(true)),
            (505, unavailable(false)),
            (599, unavailable(false)),
            (400, CatalogFailure::HttpStatus),
            (302, CatalogFailure::HttpStatus),
        ] {
            assert_eq!(failure_for_http_status(status), expected, "{status}");
        }
    }

    #[test]
    fn only_rate_limits_transport_failures_and_passing_gateway_errors_are_retried() {
        let retried = [
            CatalogFailure::RateLimited,
            CatalogFailure::Transport,
            CatalogFailure::GatewayUnavailable { retryable: true },
        ];
        for failure in retried {
            assert!(failure.retryable(), "{failure:?}");
        }
        for failure in [
            CatalogFailure::Authentication,
            CatalogFailure::GatewayUnavailable { retryable: false },
            CatalogFailure::Cancellation,
            CatalogFailure::MalformedResponse,
            CatalogFailure::HttpStatus,
        ] {
            assert!(!failure.retryable(), "{failure:?}");
        }
        assert_eq!(
            CatalogFailure::GatewayUnavailable { retryable: false }.error_name(),
            "GatewayUnavailable"
        );
    }
}
