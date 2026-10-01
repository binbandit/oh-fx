#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogFailure {
    Authentication,
    RateLimited,
    GatewayUnavailable,
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
            Self::GatewayUnavailable => "gateway_unavailable",
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
            Self::GatewayUnavailable => "GatewayUnavailable",
            Self::Cancellation => "Cancelled",
            Self::Transport => "TransportFailure",
            Self::MalformedResponse => "MalformedResponse",
            Self::HttpStatus => "Unavailable",
        }
    }
}

pub(crate) fn failure_for_http_status(status: u16) -> CatalogFailure {
    match status {
        401 | 403 => CatalogFailure::Authentication,
        429 => CatalogFailure::RateLimited,
        500..=599 => CatalogFailure::GatewayUnavailable,
        _ => CatalogFailure::HttpStatus,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_http_failures_follow_the_upstream_categories() {
        for (status, expected) in [
            (401, CatalogFailure::Authentication),
            (403, CatalogFailure::Authentication),
            (429, CatalogFailure::RateLimited),
            (500, CatalogFailure::GatewayUnavailable),
            (501, CatalogFailure::GatewayUnavailable),
            (400, CatalogFailure::HttpStatus),
            (302, CatalogFailure::HttpStatus),
        ] {
            assert_eq!(failure_for_http_status(status), expected, "{status}");
        }
    }
}
