use ofx_images::MAX_RESULT_FRAME_BYTES;
use std::sync::Arc;

use tokio::time::Instant;

use crate::error::McpError;
use crate::features::tools::{Limits, ToolCallOutcome, parse_call_outcome, validate_arguments};
use crate::legacy_elicitation_runtime::{ElicitationContext, OperationElicitation};
use crate::protocol_messages::build_tool_call_request;
use crate::server_connection::McpClient;
use crate::transport::{McpTransport, ProgressSink, TransportRequest};

pub(crate) const DEFAULT_MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;
const RESPONSE_FRAME_OVERHEAD_BYTES: usize = 16 * 1024;

pub(crate) struct CallOptions {
    pub max_tool_result_bytes: usize,
    pub progress: Option<ProgressSink>,
    pub elicitation: Option<OperationElicitation>,
}

impl Default for CallOptions {
    fn default() -> Self {
        Self {
            max_tool_result_bytes: DEFAULT_MAX_TOOL_RESULT_BYTES,
            progress: None,
            elicitation: None,
        }
    }
}

pub(crate) fn response_frame_cap(max_tool_result_bytes: usize) -> usize {
    MAX_RESULT_FRAME_BYTES.max(max_tool_result_bytes.saturating_add(RESPONSE_FRAME_OVERHEAD_BYTES))
}

impl McpClient {
    pub(crate) async fn call_tool(
        &self,
        name: &str,
        arguments_json: &str,
        options: CallOptions,
        deadline: Instant,
    ) -> Result<ToolCallOutcome, McpError> {
        let limits = Limits::default();
        validate_arguments(arguments_json, limits)?;
        let id = self.transport.next_request_id()?;
        let progress_token = options.progress.is_some().then_some(id);
        let body = build_tool_call_request(id, name, arguments_json, progress_token);
        let frame_cap = response_frame_cap(options.max_tool_result_bytes);
        let elicitation = self
            .wire
            .zip(options.elicitation)
            .and_then(|(wire, operation)| {
                ElicitationContext::new(wire, operation, deadline, self.operation_timeout)
            });
        let request = TransportRequest {
            send_cancellation: true,
            progress: options.progress,
            elicitation: elicitation.map(Arc::new),
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
