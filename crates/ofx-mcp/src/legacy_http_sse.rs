use std::future::{Future, ready};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ofx_jsonrpc::{Correlator, RegisterError, RequestId, WaitError};
use reqwest::header::{ACCEPT, CONTENT_ENCODING, CONTENT_TYPE, WWW_AUTHENTICATE};
use reqwest::{Response, StatusCode, Url};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::error::McpError;
use crate::legacy_sse::{Event, Parser};
use crate::legacy_streamable_http::HttpEndpoint;
use crate::mcp_contract::HttpHeader;
use crate::protocol_messages::{build_cancellation_notification, parse_json};
use crate::streamable_http::{EndpointError, MediaType, parse_media_type, validate_endpoint};
use crate::timing::{spawn, timeout_at};
use crate::transport::{Cancellation, McpTransport, ShutdownMode, TransportRequest};

pub(crate) const SSE_PROTOCOL_VERSION: &str = "2024-11-05";
const CANCELLATION_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_ENDPOINT_EVENT_BYTES: usize = 8192;

#[derive(Debug, Clone, PartialEq)]
enum ConnectionState {
    Starting,
    Running(String),
    Failed(McpError),
    Stopped,
}

pub(crate) struct SseShared {
    http: reqwest::Client,
    discovery_url: String,
    headers: Vec<HttpHeader>,
    responses: Correlator<String, McpError>,
    max_event_bytes: AtomicUsize,
    state: watch::Sender<ConnectionState>,
    notifications: mpsc::UnboundedSender<Value>,
}

pub(crate) struct LegacySseClient {
    shared: Arc<SseShared>,
    next_request_id: AtomicU64,
    stopping: AtomicBool,
    reader: Mutex<Option<JoinHandle<()>>>,
}

pub(crate) fn validate_initialize_response(value: &Value) -> Result<(), McpError> {
    let object = value.as_object().ok_or(McpError::McpInvalidResult)?;
    let result = object.get("result").ok_or(McpError::McpNoResult)?;
    let result = result.as_object().ok_or(McpError::McpInvalidResult)?;
    match result.get("protocolVersion").and_then(Value::as_str) {
        Some(SSE_PROTOCOL_VERSION) => Ok(()),
        _ => Err(McpError::McpUnsupportedProtocolVersion),
    }
}

impl LegacySseClient {
    pub(crate) async fn connect(
        endpoint: HttpEndpoint,
        initial_max_event_bytes: usize,
        deadline: Instant,
    ) -> Result<Self, McpError> {
        let (state, _) = watch::channel(ConnectionState::Starting);
        let shared = Arc::new(SseShared {
            http: endpoint.http,
            discovery_url: endpoint.url,
            headers: endpoint.headers,
            responses: Correlator::new(),
            max_event_bytes: AtomicUsize::new(initial_max_event_bytes),
            state,
            notifications: endpoint.notifications,
        });
        let reader = spawn(reader_main(Arc::clone(&shared)));
        let client = Self {
            shared,
            next_request_id: AtomicU64::new(1),
            stopping: AtomicBool::new(false),
            reader: Mutex::new(Some(reader)),
        };
        let mut state = client.shared.state.subscribe();
        let ready = timeout_at(
            deadline,
            state.wait_for(|state| *state != ConnectionState::Starting),
        )
        .await;
        let outcome = match ready {
            Err(_) => Err(McpError::McpRequestTimedOut),
            Ok(Err(_)) => Err(McpError::McpConnectionClosed),
            Ok(Ok(state)) => match &*state {
                ConnectionState::Running(_) => Ok(()),
                ConnectionState::Failed(error) => Err(error.clone()),
                ConnectionState::Starting | ConnectionState::Stopped => {
                    Err(McpError::McpConnectionClosed)
                }
            },
        };
        outcome.map(|()| client)
    }

    fn endpoint(&self) -> Result<String, McpError> {
        match &*self.shared.state.borrow() {
            ConnectionState::Running(endpoint) => Ok(endpoint.clone()),
            _ => Err(McpError::McpConnectionClosed),
        }
    }

    async fn run_request(&self, request: TransportRequest) -> Result<String, McpError> {
        let endpoint = self.endpoint()?;
        self.shared
            .max_event_bytes
            .fetch_max(request.max_response_bytes, Ordering::Relaxed);
        let key = RequestId::Integer(
            i64::try_from(request.id).map_err(|_| McpError::McpRequestIdExhausted)?,
        );
        let pending = self
            .shared
            .responses
            .register(key)
            .map_err(|error| match error {
                RegisterError::Duplicate => McpError::McpDuplicateRequestId,
                RegisterError::Closed(_) => McpError::McpConnectionClosed,
            })?;
        let committed = AtomicBool::new(false);
        let mut cancel_on_drop = CancelOnDrop {
            shared: Arc::clone(&self.shared),
            endpoint: endpoint.clone(),
            id: request.id,
            committed: &committed,
            armed: request.send_cancellation,
        };
        let posted = timeout_at(
            request.deadline,
            self.shared
                .post_message(&endpoint, &request.body, Some(&committed)),
        )
        .await;
        let outcome = match posted {
            Err(_) => Err(McpError::McpRequestTimedOut),
            Ok(Err(error)) => Err(error),
            Ok(Ok(())) => match pending.wait_until(request.deadline).await {
                Ok(frame) if frame.len() > request.max_response_bytes => {
                    Err(McpError::McpResponseFrameTooLarge)
                }
                Ok(frame) => Ok(frame),
                Err(WaitError::TimedOut) => Err(McpError::McpRequestTimedOut),
                Err(WaitError::Failed(error)) => Err(error),
                Err(WaitError::Abandoned) => Err(McpError::McpConnectionClosed),
            },
        };
        cancel_on_drop.armed = false;
        if outcome == Err(McpError::McpRequestTimedOut)
            && request.send_cancellation
            && committed.load(Ordering::Acquire)
        {
            self.shared
                .send_cancellation(&endpoint, request.id, "McpRequestTimedOut")
                .await;
        }
        outcome
    }

    fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(reader) = lock(&self.reader).take() {
            reader.abort();
        }
        self.shared.state.send_replace(ConnectionState::Stopped);
        self.shared.responses.close(McpError::McpConnectionClosed);
    }
}

impl Drop for LegacySseClient {
    fn drop(&mut self) {
        self.stop();
    }
}

impl McpTransport for LegacySseClient {
    fn next_request_id(&self) -> Result<u64, McpError> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(McpError::McpConnectionClosed);
        }
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        if id == u64::MAX {
            return Err(McpError::McpRequestIdExhausted);
        }
        Ok(id)
    }

    fn request(
        &self,
        request: TransportRequest,
    ) -> impl Future<Output = Result<String, McpError>> + Send {
        self.run_request(request)
    }

    async fn notify(&self, body: String, deadline: Instant) -> Result<(), McpError> {
        let endpoint = self.endpoint()?;
        timeout_at(deadline, self.shared.post_message(&endpoint, &body, None))
            .await
            .map_err(|_| McpError::McpRequestTimedOut)?
    }

    fn is_running(&self) -> bool {
        matches!(&*self.shared.state.borrow(), ConnectionState::Running(_))
    }

    fn shutdown(&self, _: ShutdownMode) -> impl Future<Output = ()> + Send {
        self.stop();
        ready(())
    }
}

struct CancelOnDrop<'a> {
    shared: Arc<SseShared>,
    endpoint: String,
    id: u64,
    committed: &'a AtomicBool,
    armed: bool,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if !self.armed || !self.committed.load(Ordering::Acquire) {
            return;
        }
        Cancellation::Sse(Arc::clone(&self.shared), std::mem::take(&mut self.endpoint))
            .send_in_background(self.id);
    }
}

impl SseShared {
    async fn post_message(
        &self,
        endpoint: &str,
        body: &str,
        committed: Option<&AtomicBool>,
    ) -> Result<(), McpError> {
        let mut builder = self
            .http
            .post(endpoint)
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json");
        for header in &self.headers {
            builder = builder.header(header.name.as_str(), header.value.as_str());
        }
        if let Some(committed) = committed {
            committed.store(true, Ordering::Release);
        }
        let response = builder.body(body.to_owned()).send().await?;
        reject_redirect_or_authentication(&response)?;
        if response.status() == StatusCode::ACCEPTED {
            Ok(())
        } else {
            Err(McpError::UnexpectedHttpStatus)
        }
    }

    pub(crate) async fn send_cancellation(&self, endpoint: &str, request_id: u64, reason: &str) {
        let body = build_cancellation_notification(request_id, reason);
        let _ = timeout_at(
            Instant::now() + CANCELLATION_TIMEOUT,
            self.post_message(endpoint, &body, None),
        )
        .await;
    }

    async fn read_connection(&self) -> Result<(), McpError> {
        let mut builder = self
            .http
            .get(&self.discovery_url)
            .header(ACCEPT, "text/event-stream");
        for header in &self.headers {
            builder = builder.header(header.name.as_str(), header.value.as_str());
        }
        let mut response = builder.send().await?;
        reject_redirect_or_authentication(&response)?;
        if response.status() != StatusCode::OK {
            return Err(McpError::UnexpectedHttpStatus);
        }
        if header_text(&response, CONTENT_ENCODING.as_str())
            .is_some_and(|encoding| !encoding.eq_ignore_ascii_case("identity"))
        {
            return Err(McpError::UnsupportedContentEncoding);
        }
        let content_type =
            header_text(&response, CONTENT_TYPE.as_str()).ok_or(McpError::MissingContentType)?;
        if parse_media_type(&content_type) != Some(MediaType::EventStream) {
            return Err(McpError::UnsupportedContentType);
        }
        let mut parser = Parser::new(0, self.max_event_bytes.load(Ordering::Relaxed), 0);
        let mut events = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            parser.set_max_event_bytes(self.max_event_bytes.load(Ordering::Relaxed));
            parser.feed(&chunk, &mut events)?;
            for event in events.drain(..) {
                self.handle_event(&event)?;
            }
        }
        parser.finish()?;
        Err(McpError::McpConnectionClosed)
    }

    fn handle_event(&self, event: &Event) -> Result<(), McpError> {
        match event.kind.as_deref().unwrap_or("message") {
            "endpoint" => {
                let endpoint = resolve_message_endpoint(&self.discovery_url, &event.data)?;
                if *self.state.borrow() != ConnectionState::Starting {
                    return Err(McpError::InvalidSseEndpointEvent);
                }
                self.state.send_replace(ConnectionState::Running(endpoint));
                Ok(())
            }
            "message" => self.dispatch_message(&event.data),
            _ => Ok(()),
        }
    }

    fn dispatch_message(&self, data: &str) -> Result<(), McpError> {
        let value = parse_json(data.as_bytes()).ok_or(McpError::McpInvalidJson)?;
        let object = value.as_object().ok_or(McpError::McpInvalidJson)?;
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(McpError::McpInvalidJson);
        }
        let Some(id) = object.get("id") else {
            let _ = self.notifications.send(value);
            return Ok(());
        };
        let id = id
            .as_u64()
            .and_then(|id| i64::try_from(id).ok())
            .ok_or(McpError::UnsupportedServerRequest)?;
        if !object.contains_key("result") && !object.contains_key("error") {
            return Err(McpError::UnsupportedServerRequest);
        }
        let key = RequestId::Integer(id);
        let outcome = if data.len() > self.max_event_bytes.load(Ordering::Relaxed) {
            Err(McpError::McpResponseFrameTooLarge)
        } else {
            Ok(data.to_owned())
        };
        self.responses.resolve(&key, outcome);
        Ok(())
    }

    fn finish_reader(&self, error: McpError) {
        let stopped = *self.state.borrow() == ConnectionState::Stopped;
        if !stopped {
            self.state
                .send_replace(ConnectionState::Failed(error.clone()));
        }
        self.responses.close(error);
    }
}

async fn reader_main(shared: Arc<SseShared>) {
    let error = match shared.read_connection().await {
        Ok(()) => McpError::McpConnectionClosed,
        Err(error) => error,
    };
    shared.finish_reader(error);
}

fn resolve_message_endpoint(discovery_url: &str, endpoint_event: &str) -> Result<String, McpError> {
    if endpoint_event.is_empty() || endpoint_event.len() > MAX_ENDPOINT_EVENT_BYTES {
        return Err(McpError::InvalidSseEndpointEvent);
    }
    let base = Url::parse(discovery_url)
        .map_err(|_| McpError::Endpoint(EndpointError::InvalidEndpoint))?;
    let resolved = base
        .join(endpoint_event)
        .map_err(|_| McpError::InvalidSseEndpointEvent)?;
    if !same_origin(&base, &resolved) {
        return Err(McpError::CrossOriginSseEndpoint);
    }
    let rendered = resolved.to_string();
    validate_endpoint(&rendered)?;
    Ok(rendered)
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme().eq_ignore_ascii_case(right.scheme())
        && left
            .host_str()
            .zip(right.host_str())
            .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
        && left.port_or_known_default() == right.port_or_known_default()
}

fn reject_redirect_or_authentication(response: &Response) -> Result<(), McpError> {
    let status = response.status();
    if status.is_redirection() {
        return Err(McpError::RedirectNotAllowed);
    }
    let challenged = response.headers().contains_key(WWW_AUTHENTICATE);
    if status == StatusCode::UNAUTHORIZED || (status == StatusCode::FORBIDDEN && challenged) {
        return Err(McpError::McpAuthenticationRequired);
    }
    Ok(())
}

fn header_text(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim_matches([' ', '\t', '\r', '\n']).to_owned())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::error::McpError;
    use crate::features::tools::ToolCallOutcome;
    use crate::mcp_contract::{McpServerConfig, TransportType};
    use crate::server_connection::McpClient;
    use crate::server_transport::ConnectOptions;
    use crate::test_support::{FakeServer, RecordedRequest, Reply};
    use crate::tool_operations::CallOptions;

    type Responder = Box<dyn Fn(&RecordedRequest) -> Option<String> + Send + Sync>;

    fn message(body: &str) -> String {
        format!("event: message\ndata: {body}\n\n")
    }

    fn standard_responses(version: &'static str) -> Responder {
        Box::new(move |request| {
            let id = request.request_id()?;
            match request.method_name()?.as_str() {
                "initialize" => Some(format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"{version}","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"legacy"}}}}}}"#
                )),
                "tools/list" => Some(format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"echo","inputSchema":{{"type":"object"}}}}]}}}}"#
                )),
                "tools/call" => Some(format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"echoed"}}]}}}}"#
                )),
                _ => None,
            }
        })
    }

    async fn sse_server(endpoint_event: &'static str, responder: Responder) -> FakeServer {
        let (events, live) = mpsc::unbounded_channel();
        let live = Mutex::new(Some(live));
        let events = Mutex::new(events);
        FakeServer::start(move |request| {
            if request.method == "GET" {
                let receiver = lock(&live).take();
                let sender = lock(&events).clone();
                let _ = sender.send(format!("event: endpoint\ndata: {endpoint_event}\n\n"));
                return receiver.map_or_else(|| Reply::status(500), Reply::live_events);
            }
            if let Some(body) = responder(request) {
                let _ = lock(&events).send(message(&body));
            }
            Reply::status(202)
        })
        .await
    }

    fn legacy(url: &str) -> McpServerConfig {
        McpServerConfig {
            startup_timeout_ms: 5_000,
            ..McpServerConfig::remote("legacy", TransportType::Sse, url)
        }
    }

    #[tokio::test]
    async fn legacy_sse_servers_initialize_at_2024_11_05_and_answer_on_the_stream() {
        let server = sse_server("/messages?session=1", standard_responses("2024-11-05")).await;
        let client = McpClient::connect(&legacy(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        assert_eq!(client.server_info().protocol_version, "2024-11-05");
        assert_eq!(client.tool_catalog().tools[0].name, "echo");
        let outcome = client
            .call_tool(
                "echo",
                r#"{"text": "hi"}"#,
                CallOptions::default(),
                Instant::now() + Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ToolCallOutcome::Complete(_)));
        client.shutdown(ShutdownMode::Graceful).await;
        let requests = server.requests();
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].header("accept"), Some("text/event-stream"));
        let posts: Vec<_> = requests
            .iter()
            .filter(|request| request.method == "POST")
            .collect();
        assert!(
            posts
                .iter()
                .all(|request| request.path == "/messages?session=1")
        );
        assert!(posts[0].body.contains(
            "\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"oh-fx\""
        ));
        assert_eq!(
            posts[1].body,
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}"
        );
        assert_eq!(
            posts[0].header("accept"),
            Some("application/json, text/event-stream")
        );
    }

    #[tokio::test]
    async fn legacy_sse_servers_must_answer_exactly_2024_11_05() {
        let server = sse_server("/messages", standard_responses("2025-03-26")).await;
        let failure = McpClient::connect(&legacy(&server.url), &ConnectOptions::default())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error, McpError::McpUnsupportedProtocolVersion);
    }

    #[tokio::test]
    async fn cross_origin_endpoints_fail_startup() {
        let server = sse_server(
            "https://elsewhere.test/messages",
            standard_responses("2024-11-05"),
        )
        .await;
        let failure = McpClient::connect(&legacy(&server.url), &ConnectOptions::default())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error, McpError::CrossOriginSseEndpoint);
    }

    #[tokio::test]
    async fn rejected_discovery_streams_report_authentication() {
        let server = FakeServer::start(|_| Reply::status(401)).await;
        let failure = McpClient::connect(&legacy(&server.url), &ConnectOptions::default())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error, McpError::McpAuthenticationRequired);
    }

    #[tokio::test]
    async fn server_requests_on_the_stream_fail_the_connection() {
        let responder: Responder = Box::new(|request| {
            let standard = standard_responses("2024-11-05");
            if request.method_name().as_deref() == Some("tools/call") {
                return Some(
                    r#"{"jsonrpc":"2.0","id":7,"method":"roots/list","params":{}}"#.to_owned(),
                );
            }
            standard(request)
        });
        let server = sse_server("/messages", responder).await;
        let client = McpClient::connect(&legacy(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        assert_eq!(
            client
                .call_tool(
                    "echo",
                    "{}",
                    CallOptions::default(),
                    Instant::now() + Duration::from_secs(10)
                )
                .await,
            Err(McpError::UnsupportedServerRequest)
        );
        assert!(!client.is_running());
        client.shutdown(ShutdownMode::Graceful).await;
    }

    #[test]
    fn http_sse_endpoint_resolution_stays_same_origin() {
        assert_eq!(
            resolve_message_endpoint("https://example.test/events/sse", "../messages?session=one"),
            Ok("https://example.test/messages?session=one".to_owned())
        );
        assert_eq!(
            resolve_message_endpoint(
                "http://127.0.0.1:4321/sse",
                "http://127.0.0.1:4321/messages"
            ),
            Ok("http://127.0.0.1:4321/messages".to_owned())
        );
        assert_eq!(
            resolve_message_endpoint("https://example.test/sse", "https://other.test/messages"),
            Err(McpError::CrossOriginSseEndpoint)
        );
        assert_eq!(
            resolve_message_endpoint("https://example.test/sse", ""),
            Err(McpError::InvalidSseEndpointEvent)
        );
        assert_eq!(
            resolve_message_endpoint("https://example.test/sse", &"x".repeat(8193)),
            Err(McpError::InvalidSseEndpointEvent)
        );
    }

    #[test]
    fn http_sse_initialize_response_requires_the_2024_protocol_version() {
        assert_eq!(
            validate_initialize_response(
                &json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}})
            ),
            Ok(())
        );
        assert_eq!(
            validate_initialize_response(&json!({"jsonrpc":"2.0","id":1,"result":{}})),
            Err(McpError::McpUnsupportedProtocolVersion)
        );
        assert_eq!(
            validate_initialize_response(
                &json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}})
            ),
            Err(McpError::McpUnsupportedProtocolVersion)
        );
    }
}
