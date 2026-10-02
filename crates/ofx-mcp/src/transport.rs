use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::time::Instant;

use crate::error::McpError;

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProgressNotification {
    pub progress: f64,
    pub total: Option<f64>,
    pub message: Option<String>,
}

pub(crate) type ProgressSink = Arc<dyn Fn(ProgressNotification) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerRequestPolicy {
    Reject,
    RefuseElicitation,
}

#[derive(Clone)]
pub(crate) struct TransportRequest {
    pub id: u64,
    pub body: String,
    pub max_response_bytes: usize,
    pub deadline: Instant,
    pub send_cancellation: bool,
    pub progress: Option<ProgressSink>,
    pub server_requests: ServerRequestPolicy,
}

impl TransportRequest {
    pub(crate) fn new(id: u64, body: String, max_response_bytes: usize, deadline: Instant) -> Self {
        Self {
            id,
            body,
            max_response_bytes,
            deadline,
            send_cancellation: false,
            progress: None,
            server_requests: ServerRequestPolicy::Reject,
        }
    }
}

impl fmt::Debug for TransportRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransportRequest")
            .field("id", &self.id)
            .field("body", &self.body)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("deadline", &self.deadline)
            .field("send_cancellation", &self.send_cancellation)
            .field("progress", &self.progress.is_some())
            .field("server_requests", &self.server_requests)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownMode {
    Graceful,
    Immediate,
    ProcessExit,
}

pub(crate) trait McpTransport: Send + Sync {
    fn next_request_id(&self) -> Result<u64, McpError>;

    fn request(&self, request: TransportRequest) -> BoxFuture<'_, Result<String, McpError>>;

    fn notify(&self, body: String, deadline: Instant) -> BoxFuture<'_, Result<(), McpError>>;

    fn is_running(&self) -> bool;

    fn shutdown(&self, mode: ShutdownMode) -> BoxFuture<'_, ()>;
}
