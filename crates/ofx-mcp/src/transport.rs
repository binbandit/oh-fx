use std::fmt;
use std::future::Future;
use std::sync::Arc;

use tokio::time::Instant;

use crate::error::McpError;
use crate::legacy_http_sse::{LegacySseClient, SseShared};
use crate::legacy_streamable_http::{HttpShared, LegacyHttpClient};
use crate::stdio_dispatcher::{Shared as StdioShared, StdioDispatcher};
use crate::timing::spawn_on;

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

    fn request(
        &self,
        request: TransportRequest,
    ) -> impl Future<Output = Result<String, McpError>> + Send;

    fn notify(
        &self,
        body: String,
        deadline: Instant,
    ) -> impl Future<Output = Result<(), McpError>> + Send;

    fn is_running(&self) -> bool;

    fn shutdown(&self, mode: ShutdownMode) -> impl Future<Output = ()> + Send;
}

pub(crate) enum Transport {
    Stdio(StdioDispatcher),
    Http(LegacyHttpClient),
    Sse(LegacySseClient),
}

impl McpTransport for Transport {
    fn next_request_id(&self) -> Result<u64, McpError> {
        match self {
            Self::Stdio(transport) => transport.next_request_id(),
            Self::Http(transport) => transport.next_request_id(),
            Self::Sse(transport) => transport.next_request_id(),
        }
    }

    async fn request(&self, request: TransportRequest) -> Result<String, McpError> {
        match self {
            Self::Stdio(transport) => transport.request(request).await,
            Self::Http(transport) => transport.request(request).await,
            Self::Sse(transport) => transport.request(request).await,
        }
    }

    async fn notify(&self, body: String, deadline: Instant) -> Result<(), McpError> {
        match self {
            Self::Stdio(transport) => transport.notify(body, deadline).await,
            Self::Http(transport) => transport.notify(body, deadline).await,
            Self::Sse(transport) => transport.notify(body, deadline).await,
        }
    }

    fn is_running(&self) -> bool {
        match self {
            Self::Stdio(transport) => transport.is_running(),
            Self::Http(transport) => transport.is_running(),
            Self::Sse(transport) => transport.is_running(),
        }
    }

    async fn shutdown(&self, mode: ShutdownMode) {
        match self {
            Self::Stdio(transport) => transport.shutdown(mode).await,
            Self::Http(transport) => transport.shutdown(mode).await,
            Self::Sse(transport) => transport.shutdown(mode).await,
        }
    }
}

pub(crate) enum Cancellation {
    Stdio(Arc<StdioShared>),
    Http(Arc<HttpShared>),
    Sse(Arc<SseShared>, String),
}

impl Cancellation {
    pub(crate) fn send_in_background(self, id: u64) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            spawn_on(&runtime, self.send(id));
        }
    }

    async fn send(self, id: u64) {
        match self {
            Self::Stdio(shared) => shared.send_cancellation(id, CANCELLED).await,
            Self::Http(shared) => shared.send_cancellation(id, CANCELLED).await,
            Self::Sse(shared, endpoint) => {
                shared.send_cancellation(&endpoint, id, CANCELLED).await;
            }
        }
    }
}

const CANCELLED: &str = "Cancelled";
