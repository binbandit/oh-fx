use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::Value;
use tokio::time::Instant;

use crate::error::McpError;
use crate::features::tools::{
    Limits, ToolCallOutcome, ToolCatalog, parse_call_outcome, validate_arguments,
};
use crate::protocol_messages::build_tool_call_request;
use crate::server_connection::{McpClient, lock};
use crate::server_transport::discover_tools;
use crate::transport::{McpTransport, ProgressSink, ServerRequestPolicy, TransportRequest};

pub(crate) const DEFAULT_MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;
const MAX_RESULT_FRAME_BYTES: usize = 8 * 1024 * 1024;
const RESPONSE_FRAME_OVERHEAD_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub(crate) struct CallOptions {
    pub max_tool_result_bytes: usize,
    pub progress: Option<ProgressSink>,
}

impl Default for CallOptions {
    fn default() -> Self {
        Self {
            max_tool_result_bytes: DEFAULT_MAX_TOOL_RESULT_BYTES,
            progress: None,
        }
    }
}

pub(crate) fn response_frame_cap(max_tool_result_bytes: usize) -> usize {
    MAX_RESULT_FRAME_BYTES.max(max_tool_result_bytes.saturating_add(RESPONSE_FRAME_OVERHEAD_BYTES))
}

impl McpClient {
    pub(crate) async fn list_tools(&self) -> Result<Arc<ToolCatalog>, McpError> {
        let deadline = Instant::now() + self.operation_timeout;
        let catalog = Arc::new(discover_tools(&self.transport, deadline, |error| error).await?);
        *lock(&self.catalog) = Arc::clone(&catalog);
        self.tools_stale.store(false, Ordering::Release);
        Ok(catalog)
    }

    pub(crate) async fn current_tools(&self) -> Result<Arc<ToolCatalog>, McpError> {
        self.receive_pending_notifications();
        if self.tools_stale.load(Ordering::Acquire) {
            return self.list_tools().await;
        }
        Ok(self.tool_catalog())
    }

    pub(crate) async fn call_tool(
        &self,
        name: &str,
        arguments: &Value,
        options: CallOptions,
    ) -> Result<ToolCallOutcome, McpError> {
        let limits = Limits::default();
        validate_arguments(arguments, limits)?;
        let arguments = arguments.as_object().ok_or(McpError::InvalidJson)?;
        let deadline = Instant::now() + self.operation_timeout;
        let id = self.transport.next_request_id()?;
        let progress_token = options.progress.is_some().then_some(id);
        let body = build_tool_call_request(id, name, arguments, progress_token);
        let frame_cap = response_frame_cap(options.max_tool_result_bytes);
        let request = TransportRequest {
            send_cancellation: true,
            progress: options.progress,
            server_requests: if self.wire.is_some() {
                ServerRequestPolicy::RefuseElicitation
            } else {
                ServerRequestPolicy::Reject
            },
            ..TransportRequest::new(id, body, frame_cap, deadline)
        };
        let response = self.transport.request(request).await?;
        parse_call_outcome(
            &response,
            options.max_tool_result_bytes.max(MAX_RESULT_FRAME_BYTES),
            limits,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_frames_allow_envelope_overhead_above_the_result_cap() {
        assert_eq!(response_frame_cap(64 * 1024), MAX_RESULT_FRAME_BYTES);
        assert_eq!(
            response_frame_cap(16 * 1024 * 1024),
            16 * 1024 * 1024 + RESPONSE_FRAME_OVERHEAD_BYTES
        );
    }
}
