use std::io;
use std::sync::Arc;

use ofx_jsonrpc::RpcError;

use crate::mcp_contract::InvalidServerConfig;
use crate::streamable_http::{EndpointError, HeaderError};

#[derive(Debug, Clone, thiserror::Error)]
pub enum McpError {
    #[error("Cancelled")]
    Cancelled,
    #[error("McpRequestTimedOut")]
    McpRequestTimedOut,
    #[error("McpConnectionClosed")]
    McpConnectionClosed,
    #[error("McpResponseFrameTooLarge")]
    McpResponseFrameTooLarge,
    #[error("McpIncompleteBody")]
    McpIncompleteBody,
    #[error("McpInvalidJson")]
    McpInvalidJson,
    #[error("McpInvalidProgress")]
    McpInvalidProgress,
    #[error("McpUnsupportedResponseId")]
    McpUnsupportedResponseId,
    #[error("McpDuplicateRequestId")]
    McpDuplicateRequestId,
    #[error("McpRequestIdExhausted")]
    McpRequestIdExhausted,
    #[error("McpWriteInterrupted")]
    McpWriteInterrupted,
    #[error("McpUnsupportedProtocolVersion")]
    McpUnsupportedProtocolVersion,
    #[error("McpInitFailed")]
    McpInitFailed,
    #[error("McpServerExitedDuringStartup")]
    McpServerExitedDuringStartup,
    #[error("McpToolDiscoveryFailed")]
    McpToolDiscoveryFailed,
    #[error("McpNoResult")]
    McpNoResult,
    #[error("McpInvalidResult")]
    McpInvalidResult,
    #[error("McpInvalidProtocolError")]
    McpInvalidProtocolError,
    #[error("McpProtocolError")]
    McpProtocolError(RpcError),
    #[error("McpInvalidServerConfig")]
    McpInvalidServerConfig,
    #[error("McpWorkspaceApprovalRequired")]
    McpWorkspaceApprovalRequired,
    #[error("McpHeaderEnvironmentMissing")]
    McpHeaderEnvironmentMissing,
    #[error("McpBearerEnvironmentMissing")]
    McpBearerEnvironmentMissing,
    #[error("McpSessionExpired")]
    McpSessionExpired,
    #[error("McpAuthenticationRequired")]
    McpAuthenticationRequired,
    #[error("McpNotificationListenerUnsupported")]
    McpNotificationListenerUnsupported,
    #[error("InvalidMcpSessionId")]
    InvalidMcpSessionId,
    #[error("UnexpectedHttpStatus")]
    UnexpectedHttpStatus,
    #[error("HttpClientUnavailable")]
    HttpClientUnavailable,
    #[error("RedirectNotAllowed")]
    RedirectNotAllowed,
    #[error("UnsupportedContentEncoding")]
    UnsupportedContentEncoding,
    #[error("MissingContentType")]
    MissingContentType,
    #[error("UnsupportedContentType")]
    UnsupportedContentType,
    #[error("ResponseTooLarge")]
    ResponseTooLarge,
    #[error("SseEventTooLarge")]
    SseEventTooLarge,
    #[error("TooManySseEvents")]
    TooManySseEvents,
    #[error("BrokenSseStream")]
    BrokenSseStream,
    #[error("InvalidSseEvent")]
    InvalidSseEvent,
    #[error("MismatchedResponseId")]
    MismatchedResponseId,
    #[error("UnsupportedServerRequest")]
    UnsupportedServerRequest,
    #[error("MissingFinalResponse")]
    MissingFinalResponse,
    #[error("InvalidSseEndpointEvent")]
    InvalidSseEndpointEvent,
    #[error("CrossOriginSseEndpoint")]
    CrossOriginSseEndpoint,
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[error(transparent)]
    Header(#[from] HeaderError),
    #[error("InvalidEnvelope")]
    InvalidEnvelope,
    #[error("ProtocolFailure")]
    ProtocolFailure,
    #[error("InvalidListResult")]
    InvalidListResult,
    #[error("InvalidTool")]
    InvalidTool,
    #[error("DuplicateTool")]
    DuplicateTool,
    #[error("DuplicateCursor")]
    DuplicateCursor,
    #[error("InconsistentCacheScope")]
    InconsistentCacheScope,
    #[error("PaginationLimitExceeded")]
    PaginationLimitExceeded,
    #[error("ToolLimitExceeded")]
    ToolLimitExceeded,
    #[error("InvalidCallResult")]
    InvalidCallResult,
    #[error("UnsupportedResultType")]
    UnsupportedResultType,
    #[error("InvalidContent")]
    InvalidContent,
    #[error("InvalidSchema")]
    InvalidSchema,
    #[error("UnsupportedDialect")]
    UnsupportedDialect,
    #[error("SchemaLimitExceeded")]
    SchemaLimitExceeded,
    #[error("InstanceLimitExceeded")]
    InstanceLimitExceeded,
    #[error("InvalidJson")]
    InvalidJson,
    #[error("MetadataLimitExceeded")]
    MetadataLimitExceeded,
    #[error("{}", io_error_name(.0))]
    Io(Arc<io::Error>),
    #[error("{}", http_error_name(.0))]
    Http(Arc<reqwest::Error>),
}

fn io_error_name(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::NotFound => "FileNotFound",
        io::ErrorKind::PermissionDenied => "AccessDenied",
        io::ErrorKind::BrokenPipe => "BrokenPipe",
        io::ErrorKind::ConnectionRefused => "ConnectionRefused",
        io::ErrorKind::ConnectionReset => "ConnectionResetByPeer",
        io::ErrorKind::TimedOut => "Timeout",
        io::ErrorKind::InvalidData => "InvalidData",
        _ => "Unexpected",
    }
}

fn http_error_name(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "Timeout"
    } else if error.is_connect() {
        "ConnectionRefused"
    } else if error.is_redirect() {
        "RedirectNotAllowed"
    } else if error.is_body() || error.is_decode() {
        "ReadFailed"
    } else {
        "HttpRequestFailed"
    }
}

impl From<io::Error> for McpError {
    fn from(error: io::Error) -> Self {
        Self::Io(Arc::new(error))
    }
}

impl From<reqwest::Error> for McpError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(Arc::new(error))
    }
}

impl From<InvalidServerConfig> for McpError {
    fn from(_: InvalidServerConfig) -> Self {
        Self::McpInvalidServerConfig
    }
}

impl PartialEq for McpError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::McpProtocolError(left), Self::McpProtocolError(right)) => left == right,
            (Self::Endpoint(left), Self::Endpoint(right)) => left == right,
            (Self::Header(left), Self::Header(right)) => left == right,
            (Self::Io(left), Self::Io(right)) => Arc::ptr_eq(left, right),
            (Self::Http(left), Self::Http(right)) => Arc::ptr_eq(left, right),
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use ofx_http::ConnectionOptions;

    use super::*;

    #[test]
    fn io_failures_display_their_name_without_the_detail() {
        let missing = McpError::from(io::Error::new(io::ErrorKind::NotFound, "/private/server"));
        assert_eq!(missing.to_string(), "FileNotFound");
    }

    #[tokio::test]
    async fn http_failures_never_display_the_request_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let client = ofx_http::build_connection_client(&ConnectionOptions::default()).unwrap();
        let failure = client
            .get(format!("http://127.0.0.1:{port}/mcp?token=secret"))
            .send()
            .await
            .unwrap_err();
        let error = McpError::from(failure);
        assert_eq!(error.to_string(), "ConnectionRefused");
    }
}
