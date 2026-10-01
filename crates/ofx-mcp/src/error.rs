use std::io;
use std::sync::Arc;

use ofx_jsonrpc::RpcError;

use crate::mcp_contract::InvalidServerConfig;

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
    #[error("{0}")]
    Io(Arc<io::Error>),
}

impl From<io::Error> for McpError {
    fn from(error: io::Error) -> Self {
        Self::Io(Arc::new(error))
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
            (Self::Io(left), Self::Io(right)) => Arc::ptr_eq(left, right),
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}
