use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ofx_jsonrpc::{ErrorCode, Frame, RequestId, RpcError};
use serde_json::Value;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::elicitation::{ElicitationCapabilities, Request, parse_request, validate_answer};
use crate::elicitation_interaction::{InputResponder, Interaction};
use crate::error::McpError;
use crate::operation_control::DeadlineGate;
use crate::protocol_messages::parse_json;
use crate::protocol_negotiation::ElicitationWire;

const MAX_SERVER_REQUESTS: usize = 32;
const MAX_SERVER_REQUEST_FRAME_BYTES: usize = 128 * 1024;
const MAX_STRING_ID_BYTES: usize = 256;

pub(crate) trait ElicitationOwner: Send + Sync {
    fn current(&self) -> bool;
}

pub(crate) struct ElicitationContext {
    wire: ElicitationWire,
    capabilities: ElicitationCapabilities,
    responder: InputResponder,
    server_name: String,
    owner: Box<dyn ElicitationOwner>,
    gate: DeadlineGate,
    operation_timeout: Duration,
    finished: CancellationToken,
    requests_seen: AtomicUsize,
    active: watch::Sender<usize>,
}

pub(crate) struct Answering(Arc<ElicitationContext>);

impl Answering {
    pub(crate) fn context(&self) -> &ElicitationContext {
        &self.0
    }
}

impl Drop for Answering {
    fn drop(&mut self) {
        self.0.active.send_modify(|active| *active -= 1);
    }
}

pub(crate) struct OperationElicitation {
    pub(crate) responder: InputResponder,
    pub(crate) server_name: String,
    pub(crate) owner: Box<dyn ElicitationOwner>,
}

impl ElicitationContext {
    pub(crate) fn new(
        wire: ElicitationWire,
        operation: OperationElicitation,
        deadline: Instant,
        operation_timeout: Duration,
    ) -> Option<Self> {
        let capabilities = operation.responder.capabilities();
        capabilities.any().then(|| Self {
            wire,
            capabilities,
            responder: operation.responder,
            server_name: operation.server_name,
            owner: operation.owner,
            gate: DeadlineGate::new(deadline),
            operation_timeout,
            finished: CancellationToken::new(),
            requests_seen: AtomicUsize::new(0),
            active: watch::Sender::new(0),
        })
    }

    pub(crate) fn begin_answer(self: &Arc<Self>) -> Answering {
        self.active.send_modify(|active| *active += 1);
        Answering(Arc::clone(self))
    }

    pub(crate) async fn answered(&self) {
        let _ = self
            .active
            .subscribe()
            .wait_for(|active| *active == 0)
            .await;
    }

    pub(crate) fn operation_timeout(&self) -> Duration {
        self.operation_timeout
    }

    pub(crate) fn gate(&self) -> &DeadlineGate {
        &self.gate
    }

    pub(crate) fn finish(&self) {
        self.finished.cancel();
    }

    pub(crate) async fn respond(&self, frame: &[u8]) -> Result<String, McpError> {
        if frame.len() > MAX_SERVER_REQUEST_FRAME_BYTES {
            return Err(McpError::McpResponseFrameTooLarge);
        }
        let request = parse_json(frame).ok_or(McpError::McpInvalidJson)?;
        let object = request.as_object().ok_or(McpError::McpInvalidJson)?;
        let id = object.get("id").ok_or(McpError::McpInvalidJson)?;
        if !valid_server_request_id(id) {
            return Ok(error_response(
                &Value::Null,
                ErrorCode::INVALID_REQUEST,
                "Invalid request id",
            ));
        }
        if self.requests_seen.fetch_add(1, Ordering::Relaxed) >= MAX_SERVER_REQUESTS {
            return Ok(error_response(
                id,
                ErrorCode::INTERNAL_ERROR,
                "Elicitation request limit exceeded",
            ));
        }
        if object.get("method").and_then(Value::as_str) != Some("elicitation/create") {
            return Ok(error_response(
                id,
                ErrorCode::METHOD_NOT_FOUND,
                "Method not found",
            ));
        }
        let Some(request) = object
            .get("params")
            .and_then(|params| parse_request(self.wire, params).ok())
        else {
            return Ok(error_response(
                id,
                ErrorCode::INVALID_PARAMS,
                "Invalid params",
            ));
        };
        if !self.capabilities.supports(request.mode()) {
            return Ok(error_response(
                id,
                ErrorCode::INVALID_PARAMS,
                "Unsupported elicitation mode",
            ));
        }
        let deadline = self.gate.pause_for_input();
        let response = self.answer(id, &request, deadline).await;
        self.gate.resume(self.operation_timeout);
        Ok(response)
    }

    async fn answer(&self, id: &Value, request: &Request, deadline: Instant) -> String {
        if !self.owner.current() {
            return owner_changed(id);
        }
        let interaction = Interaction {
            responder: &self.responder,
            server_name: &self.server_name,
            deadline,
            finished: &self.finished,
        };
        let response = match interaction.respond(request).await {
            Ok(response) => response,
            Err(error) => {
                return error_response(id, ErrorCode::INTERNAL_ERROR, &error.to_string());
            }
        };
        if !self.owner.current() {
            return owner_changed(id);
        }
        if validate_answer(request, &response).is_err() {
            return error_response(
                id,
                ErrorCode::INTERNAL_ERROR,
                "Invalid elicitation response",
            );
        }
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{}}}",
            response.to_json()
        )
    }
}

fn owner_changed(id: &Value) -> String {
    error_response(id, ErrorCode::INTERNAL_ERROR, "Elicitation owner changed")
}

fn valid_server_request_id(value: &Value) -> bool {
    match value {
        Value::Number(number) => number.is_i64(),
        Value::String(text) => text.len() <= MAX_STRING_ID_BYTES,
        _ => false,
    }
}

pub(crate) fn method_not_found_response(frame: &[u8]) -> String {
    let id = parse_json(frame)
        .and_then(|request| request.get("id").cloned())
        .unwrap_or(Value::Null);
    error_response(&id, ErrorCode::METHOD_NOT_FOUND, "Method not found")
}

pub(crate) fn server_request_failed_response(frame: &[u8]) -> String {
    let id = parse_json(frame)
        .and_then(|request| request.get("id").cloned())
        .unwrap_or(Value::Null);
    error_response(&id, ErrorCode::INTERNAL_ERROR, "Server request failed")
}

fn error_response(id: &Value, code: i64, message: &str) -> String {
    let id = RequestId::from_value(id).unwrap_or(RequestId::Null);
    Frame::ErrorResponse {
        id: &id,
        error: &RpcError::new(code, message),
    }
    .encode()
}

#[cfg(test)]
mod tests;
