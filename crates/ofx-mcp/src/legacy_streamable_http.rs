use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_ENCODING, CONTENT_TYPE, WWW_AUTHENTICATE};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::error::McpError;
use crate::legacy_elicitation_runtime::ElicitationContext;
use crate::legacy_sse::{Event, Parser};
use crate::protocol_messages::{build_cancellation_notification, parse_json};
use crate::protocol_negotiation::ElicitationWire;
use crate::server_auth::HttpAuth;
use crate::streamable_http::{MediaType, parse_media_type, validate_header_value};
use crate::timing::{sleep, spawn, timeout, timeout_at};
use crate::transport::{
    Cancellation, McpTransport, ProgressNotification, ProgressSink, ServerRequestPolicy,
    ShutdownMode, TransportRequest,
};

pub(crate) const HTTP_INITIALIZED_NOTIFICATION: &str =
    "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}";
const MAX_SSE_EVENTS: usize = 1024;
const CLOSE_TIMEOUT: Duration = Duration::from_millis(250);
const CANCELLATION_TIMEOUT: Duration = Duration::from_millis(100);
const SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const NOTIFICATION_FRAME_LIMIT: usize = 64 * 1024;
const MAX_SESSION_ID_BYTES: usize = 1024;
const RETRY_INITIAL: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(5);
const RETRY_MAX_ATTEMPT: u32 = 8;
const ACCEPT_JSON_AND_EVENTS: &str = "application/json, text/event-stream";
const ACCEPT_EVENTS: &str = "text/event-stream";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HttpVersion {
    V2025_11_25,
    V2025_06_18,
    V2025_03_26,
}

impl HttpVersion {
    pub(crate) const PREFERRED: Self = Self::V2025_11_25;

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::V2025_11_25 => "2025-11-25",
            Self::V2025_06_18 => "2025-06-18",
            Self::V2025_03_26 => "2025-03-26",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [Self::V2025_11_25, Self::V2025_06_18, Self::V2025_03_26]
            .into_iter()
            .find(|version| version.as_str() == value)
    }

    fn sends_protocol_header(self) -> bool {
        self != Self::V2025_03_26
    }

    fn allows_polling_close(self) -> bool {
        self == Self::V2025_11_25
    }

    pub(crate) fn wire(self) -> Option<ElicitationWire> {
        match self {
            Self::V2025_11_25 => Some(ElicitationWire::LegacyMcp2025_11),
            Self::V2025_06_18 => Some(ElicitationWire::LegacyMcp2025_06),
            Self::V2025_03_26 => None,
        }
    }
}

pub(crate) fn validate_session_id(value: &str) -> Result<(), McpError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_SESSION_ID_BYTES
        && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte));
    if valid {
        Ok(())
    } else {
        Err(McpError::InvalidMcpSessionId)
    }
}

pub(crate) struct HttpShared {
    http: reqwest::Client,
    url: String,
    auth: Arc<HttpAuth>,
    version: Option<HttpVersion>,
    session_id: Option<String>,
    stopping: AtomicBool,
    session_retired: AtomicBool,
    notifications: mpsc::UnboundedSender<Value>,
}

struct PostOptions<'a> {
    request_id: u64,
    max_response_bytes: usize,
    progress: Option<&'a ProgressSink>,
    elicitation: Option<&'a ElicitationContext>,
    committed: Option<&'a AtomicBool>,
    capture_session: bool,
}

impl PostOptions<'_> {
    fn new(request_id: u64, max_response_bytes: usize) -> Self {
        Self {
            request_id,
            max_response_bytes,
            progress: None,
            elicitation: None,
            committed: None,
            capture_session: false,
        }
    }
}

struct FinalResponse {
    body: String,
    session_id: Option<String>,
}

enum StreamResult {
    Final(String),
    Resumable {
        last_event_id: String,
        retry_ms: u32,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum EventOutcome {
    Empty,
    Notification,
    ProgressNotification,
    ServerRequest,
    Final,
}

#[derive(Default)]
struct StreamCursor {
    last_event_id: Option<String>,
    retry_ms: u32,
}

pub(crate) struct LegacyHttpClient {
    shared: Arc<HttpShared>,
    version: HttpVersion,
    next_request_id: AtomicU64,
    listener: Mutex<Option<JoinHandle<()>>>,
}

pub(crate) struct HttpEndpoint {
    pub(crate) http: reqwest::Client,
    pub(crate) url: String,
    pub(crate) auth: Arc<HttpAuth>,
    pub(crate) notifications: mpsc::UnboundedSender<Value>,
}

impl LegacyHttpClient {
    pub(crate) async fn initialize(
        endpoint: HttpEndpoint,
        body: &str,
        request_id: u64,
        max_response_bytes: usize,
    ) -> Result<(Self, Value), McpError> {
        let bootstrap = HttpShared {
            http: endpoint.http,
            url: endpoint.url,
            auth: endpoint.auth,
            version: None,
            session_id: None,
            stopping: AtomicBool::new(false),
            session_retired: AtomicBool::new(false),
            notifications: endpoint.notifications,
        };
        let options = PostOptions {
            capture_session: true,
            ..PostOptions::new(request_id, max_response_bytes)
        };
        let FinalResponse { body, session_id } = bootstrap.post(body, &options).await?;
        let value = parse_json(body.as_bytes()).ok_or(McpError::McpInvalidJson)?;
        let version = initialized_version(&value)?;
        if let Some(session_id) = &session_id {
            validate_session_id(session_id)?;
        }
        let shared = HttpShared {
            version: Some(version),
            session_id,
            ..bootstrap
        };
        let client = Self {
            shared: Arc::new(shared),
            version,
            next_request_id: AtomicU64::new(request_id + 1),
            listener: Mutex::new(None),
        };
        Ok((client, value))
    }

    pub(crate) fn version(&self) -> HttpVersion {
        self.version
    }

    pub(crate) fn listening(&self) -> bool {
        lock(&self.listener)
            .as_ref()
            .is_some_and(|listener| !listener.is_finished())
    }

    pub(crate) fn start_notification_listener(&self) {
        let shared = Arc::clone(&self.shared);
        *lock(&self.listener) = Some(spawn(listener_main(shared)));
    }

    async fn run_request(&self, request: TransportRequest) -> Result<String, McpError> {
        if self.shared.stopping.load(Ordering::Acquire) {
            return Err(McpError::McpConnectionClosed);
        }
        let committed = AtomicBool::new(false);
        let elicitation = (request.server_requests == ServerRequestPolicy::RefuseElicitation)
            .then(ElicitationContext::default);
        let options = PostOptions {
            progress: request.progress.as_ref(),
            elicitation: elicitation.as_ref(),
            committed: Some(&committed),
            ..PostOptions::new(request.id, request.max_response_bytes)
        };
        let mut cancel_on_drop = CancelOnDrop {
            shared: Arc::clone(&self.shared),
            id: request.id,
            committed: &committed,
            armed: request.send_cancellation,
        };
        let outcome = timeout_at(request.deadline, self.shared.post(&request.body, &options)).await;
        cancel_on_drop.armed = false;
        match outcome {
            Err(_) => {
                if request.send_cancellation && committed.load(Ordering::Acquire) {
                    self.shared
                        .send_cancellation(request.id, "McpRequestTimedOut")
                        .await;
                }
                Err(McpError::McpRequestTimedOut)
            }
            Ok(Ok(response)) => Ok(response.body),
            Ok(Err(error)) => Err(error),
        }
    }

    async fn stop(&self, mode: ShutdownMode) {
        self.shared.stopping.store(true, Ordering::Release);
        if let Some(listener) = lock(&self.listener).take() {
            listener.abort();
        }
        if mode != ShutdownMode::ProcessExit {
            self.shared.terminate_session().await;
        }
    }
}

impl Drop for LegacyHttpClient {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        if let Some(listener) = lock(&self.listener).take() {
            listener.abort();
        }
    }
}

impl McpTransport for LegacyHttpClient {
    fn next_request_id(&self) -> Result<u64, McpError> {
        if self.shared.stopping.load(Ordering::Acquire) {
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
        if self.shared.stopping.load(Ordering::Acquire) {
            return Err(McpError::McpConnectionClosed);
        }
        self.shared.send_notification(&body, deadline).await
    }

    fn is_running(&self) -> bool {
        !self.shared.stopping.load(Ordering::Acquire)
    }

    fn shutdown(&self, mode: ShutdownMode) -> impl Future<Output = ()> + Send {
        self.stop(mode)
    }
}

struct CancelOnDrop<'a> {
    shared: Arc<HttpShared>,
    id: u64,
    committed: &'a AtomicBool,
    armed: bool,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if !self.armed || !self.committed.load(Ordering::Acquire) {
            return;
        }
        Cancellation::Http(Arc::clone(&self.shared)).send_in_background(self.id);
    }
}

impl HttpShared {
    async fn builder(
        &self,
        method: Method,
        accepts_json: bool,
        last_event_id: Option<&str>,
    ) -> Result<RequestBuilder, McpError> {
        let accept = if accepts_json {
            ACCEPT_JSON_AND_EVENTS
        } else {
            ACCEPT_EVENTS
        };
        let mut builder = self.http.request(method, &self.url).header(ACCEPT, accept);
        if let Some(version) = self.version
            && version.sends_protocol_header()
        {
            builder = builder.header("MCP-Protocol-Version", version.as_str());
        }
        if let Some(session_id) = &self.session_id {
            builder = builder.header("MCP-Session-Id", session_id);
        }
        if let Some(last_event_id) = last_event_id {
            validate_header_value(last_event_id)?;
            builder = builder.header("Last-Event-ID", last_event_id);
        }
        self.auth.apply(builder).await
    }

    async fn post_builder(&self, body: &str) -> Result<RequestBuilder, McpError> {
        Ok(self
            .builder(Method::POST, true, None)
            .await?
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_owned()))
    }

    async fn post(&self, body: &str, options: &PostOptions<'_>) -> Result<FinalResponse, McpError> {
        let builder = self.post_builder(body).await?;
        if let Some(committed) = options.committed {
            committed.store(true, Ordering::Release);
        }
        let response = builder.send().await?;
        reject_redirect_or_authentication(&response)?;
        self.require_success(&response)?;
        let request_id = options.request_id;
        let session_id = options
            .capture_session
            .then(|| header_text(&response, "mcp-session-id"))
            .flatten();
        let body = match media_type(&response)? {
            MediaType::Json => {
                read_json_response(response, request_id, options.max_response_bytes).await?
            }
            MediaType::EventStream => {
                self.read_sse_with_resumption(response, request_id, options)
                    .await?
            }
        };
        Ok(FinalResponse { body, session_id })
    }

    fn require_success(&self, response: &Response) -> Result<(), McpError> {
        let status = response.status();
        if status == StatusCode::NOT_FOUND && self.session_id.is_some() {
            self.session_retired.store(true, Ordering::Release);
            return Err(McpError::McpSessionExpired);
        }
        if status != StatusCode::OK {
            return Err(McpError::UnexpectedHttpStatus);
        }
        if header_text(response, CONTENT_ENCODING.as_str())
            .is_some_and(|encoding| !encoding.eq_ignore_ascii_case("identity"))
        {
            return Err(McpError::UnsupportedContentEncoding);
        }
        Ok(())
    }

    async fn send_notification(&self, body: &str, deadline: Instant) -> Result<(), McpError> {
        timeout_at(deadline, self.post_accepted(body))
            .await
            .map_err(|_| McpError::McpRequestTimedOut)?
    }

    async fn post_accepted(&self, body: &str) -> Result<(), McpError> {
        let response = self.post_builder(body).await?.send().await?;
        reject_redirect_or_authentication(&response)?;
        if response.status() == StatusCode::ACCEPTED {
            Ok(())
        } else {
            Err(McpError::UnexpectedHttpStatus)
        }
    }

    pub(crate) async fn send_cancellation(&self, request_id: u64, reason: &str) {
        let body = build_cancellation_notification(request_id, reason);
        let _ = self
            .send_notification(&body, Instant::now() + CANCELLATION_TIMEOUT)
            .await;
    }

    async fn read_sse_with_resumption(
        &self,
        response: Response,
        request_id: u64,
        options: &PostOptions<'_>,
    ) -> Result<String, McpError> {
        let mut result = self
            .read_sse_stream(response, request_id, options, &StreamCursor::default())
            .await?;
        loop {
            match result {
                StreamResult::Final(body) => return Ok(body),
                StreamResult::Resumable {
                    last_event_id,
                    retry_ms,
                } => {
                    if retry_ms > 0 {
                        sleep(Duration::from_millis(retry_ms.into())).await;
                    }
                    let response = self
                        .builder(Method::GET, false, Some(&last_event_id))
                        .await?
                        .send()
                        .await?;
                    reject_redirect_or_authentication(&response)?;
                    self.require_success(&response)?;
                    if media_type(&response)? != MediaType::EventStream {
                        return Err(McpError::UnsupportedContentType);
                    }
                    let resumed = StreamCursor {
                        last_event_id: Some(last_event_id),
                        retry_ms,
                    };
                    result = self
                        .read_sse_stream(response, request_id, options, &resumed)
                        .await?;
                }
            }
        }
    }

    async fn read_sse_stream(
        &self,
        mut response: Response,
        request_id: u64,
        options: &PostOptions<'_>,
        resumed: &StreamCursor,
    ) -> Result<StreamResult, McpError> {
        let mut parser = Parser::new(
            options.max_response_bytes,
            options.max_response_bytes,
            MAX_SSE_EVENTS,
        );
        let mut events = Vec::new();
        let mut last_event_id: Option<String> = None;
        let mut retry_ms = resumed.retry_ms;
        let mut saw_polling_priming = false;
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(error) => {
                    let cursor = last_event_id.or_else(|| resumed.last_event_id.clone());
                    if self.version.is_some_and(|version| {
                        close_is_resumable(version, cursor.as_deref(), saw_polling_priming)
                    }) {
                        return Ok(StreamResult::Resumable {
                            last_event_id: cursor.unwrap_or_default(),
                            retry_ms,
                        });
                    }
                    return Err(error.into());
                }
            };
            parser.feed(&chunk, &mut events)?;
            for event in events.drain(..) {
                if let Some(id) = &event.id {
                    saw_polling_priming |= id.is_empty() && event.data.is_empty();
                    last_event_id = Some(id.clone());
                }
                if let Some(value) = event.retry_ms {
                    retry_ms = value;
                }
                match classify_event(&event.data, request_id, options.progress)? {
                    EventOutcome::Empty | EventOutcome::ProgressNotification => {}
                    EventOutcome::Notification => self.route_notification(&event.data)?,
                    EventOutcome::ServerRequest => {
                        self.answer_server_request(&event, options.elicitation)
                            .await?;
                    }
                    EventOutcome::Final => {
                        validate_final_response(&event.data, request_id)?;
                        return Ok(StreamResult::Final(event.data));
                    }
                }
            }
        }
        parser.finish()?;
        let version = self.version.ok_or(McpError::MissingFinalResponse)?;
        if close_is_resumable(version, last_event_id.as_deref(), saw_polling_priming) {
            return Ok(StreamResult::Resumable {
                last_event_id: last_event_id.unwrap_or_default(),
                retry_ms,
            });
        }
        Err(McpError::MissingFinalResponse)
    }

    async fn answer_server_request(
        &self,
        event: &Event,
        elicitation: Option<&ElicitationContext>,
    ) -> Result<(), McpError> {
        let context = elicitation.ok_or(McpError::UnsupportedServerRequest)?;
        let response = context.respond(event.data.as_bytes())?;
        self.send_notification(&response, Instant::now() + SERVER_REQUEST_TIMEOUT)
            .await
    }

    async fn listen_once(&self, cursor: &mut StreamCursor) -> Result<bool, McpError> {
        let mut response = self
            .builder(Method::GET, false, cursor.last_event_id.as_deref())
            .await?
            .send()
            .await?;
        reject_redirect_or_authentication(&response)?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Err(McpError::McpNotificationListenerUnsupported);
        }
        self.require_success(&response)?;
        if media_type(&response)? != MediaType::EventStream {
            return Err(McpError::UnsupportedContentType);
        }
        let mut parser = Parser::new(0, NOTIFICATION_FRAME_LIMIT, 0);
        let mut events = Vec::new();
        let mut received_events = false;
        while let Some(chunk) = response.chunk().await? {
            parser.feed(&chunk, &mut events)?;
            for event in events.drain(..) {
                received_events = true;
                if let Some(id) = event.id {
                    cursor.last_event_id = Some(id);
                }
                if let Some(value) = event.retry_ms {
                    cursor.retry_ms = value;
                }
                if !event.data.is_empty() {
                    self.route_notification(&event.data)?;
                }
            }
        }
        parser.finish()?;
        Ok(received_events)
    }

    fn route_notification(&self, data: &str) -> Result<(), McpError> {
        let value = parse_json(data.as_bytes()).ok_or(McpError::InvalidSseEvent)?;
        let object = value.as_object().ok_or(McpError::InvalidSseEvent)?;
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(McpError::InvalidSseEvent);
        }
        if object.contains_key("id") {
            return Ok(());
        }
        if !object.get("method").is_some_and(Value::is_string) {
            return Err(McpError::InvalidSseEvent);
        }
        let _ = self.notifications.send(value);
        Ok(())
    }

    async fn terminate_session(&self) {
        if self.session_id.is_none() || self.session_retired.load(Ordering::Acquire) {
            return;
        }
        let Ok(builder) = self.builder(Method::DELETE, false, None).await else {
            return;
        };
        let _ = timeout(CLOSE_TIMEOUT, builder.send()).await;
    }
}

async fn listener_main(shared: Arc<HttpShared>) {
    let mut cursor = StreamCursor::default();
    let mut attempt = 0;
    loop {
        match shared.listen_once(&mut cursor).await {
            Ok(true) => {
                attempt = 0;
                sleep(Duration::from_millis(cursor.retry_ms.into())).await;
                continue;
            }
            Err(
                McpError::McpNotificationListenerUnsupported
                | McpError::McpAuthenticationRequired
                | McpError::McpSessionExpired
                | McpError::MissingFinalResponse,
            ) => return,
            Ok(false) | Err(_) => {}
        }
        if shared.stopping.load(Ordering::Acquire) {
            return;
        }
        sleep(retry_delay(attempt)).await;
        attempt = (attempt + 1).min(RETRY_MAX_ATTEMPT);
    }
}

fn retry_delay(attempt: u32) -> Duration {
    (0..attempt.min(RETRY_MAX_ATTEMPT)).fold(RETRY_INITIAL, |delay, _| (delay * 2).min(RETRY_MAX))
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

fn media_type(response: &Response) -> Result<MediaType, McpError> {
    let content_type =
        header_text(response, CONTENT_TYPE.as_str()).ok_or(McpError::MissingContentType)?;
    parse_media_type(&content_type).ok_or(McpError::UnsupportedContentType)
}

fn header_text(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim_matches([' ', '\t', '\r', '\n']).to_owned())
}

async fn read_json_response(
    mut response: Response,
    request_id: u64,
    max_response_bytes: usize,
) -> Result<String, McpError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > max_response_bytes.saturating_sub(body.len()) {
            return Err(McpError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8(body).map_err(|_| McpError::McpInvalidJson)?;
    validate_final_response(&body, request_id)?;
    Ok(body)
}

fn initialized_version(value: &Value) -> Result<HttpVersion, McpError> {
    let object = value.as_object().ok_or(McpError::McpInvalidResult)?;
    let result = object.get("result").ok_or(McpError::McpNoResult)?;
    let result = result.as_object().ok_or(McpError::McpInvalidResult)?;
    result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .and_then(HttpVersion::parse)
        .ok_or(McpError::McpUnsupportedProtocolVersion)
}

fn close_is_resumable(
    version: HttpVersion,
    last_event_id: Option<&str>,
    saw_polling_priming: bool,
) -> bool {
    if last_event_id.is_some_and(|id| !id.is_empty()) {
        return true;
    }
    version.allows_polling_close() && saw_polling_priming
}

fn classify_event(
    data: &str,
    request_id: u64,
    progress: Option<&ProgressSink>,
) -> Result<EventOutcome, McpError> {
    if data.is_empty() {
        return Ok(EventOutcome::Empty);
    }
    let value = parse_json(data.as_bytes()).ok_or(McpError::InvalidSseEvent)?;
    let object = value.as_object().ok_or(McpError::InvalidSseEvent)?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpError::InvalidSseEvent);
    }
    if let Some(id) = object.get("id") {
        if let Some(method) = object.get("method") {
            return match method.as_str() {
                Some(method) if !method.is_empty() => Ok(EventOutcome::ServerRequest),
                _ => Err(McpError::InvalidSseEvent),
            };
        }
        if id.as_u64() != Some(request_id) {
            return Err(McpError::MismatchedResponseId);
        }
        if object.contains_key("result") || object.contains_key("error") {
            return Ok(EventOutcome::Final);
        }
        return Err(McpError::UnsupportedServerRequest);
    }
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .ok_or(McpError::InvalidSseEvent)?;
    if method != "notifications/progress" {
        return Ok(EventOutcome::Notification);
    }
    let Some(update) = request_progress(object.get("params"), request_id) else {
        return Ok(EventOutcome::Notification);
    };
    if let Some(sink) = progress {
        sink(update);
    }
    Ok(EventOutcome::ProgressNotification)
}

fn request_progress(params: Option<&Value>, request_id: u64) -> Option<ProgressNotification> {
    let params = params?.as_object()?;
    if params.get("progressToken")?.as_u64()? != request_id {
        return None;
    }
    Some(ProgressNotification {
        progress: params.get("progress")?.as_f64()?,
        total: params.get("total").and_then(Value::as_f64),
        message: params
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn validate_final_response(body: &str, request_id: u64) -> Result<(), McpError> {
    let value = parse_json(body.as_bytes()).ok_or(McpError::McpInvalidJson)?;
    let object = value.as_object().ok_or(McpError::McpInvalidJson)?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpError::McpInvalidJson);
    }
    let id = object.get("id").ok_or(McpError::McpInvalidJson)?;
    if id.as_u64() != Some(request_id) {
        return Err(McpError::MismatchedResponseId);
    }
    if !object.contains_key("result") && !object.contains_key("error") {
        return Err(McpError::McpInvalidJson);
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use serde_json::json;

    use super::*;
    use crate::features::tools::{ToolCallOutcome, ToolContent};
    use crate::mcp_contract::{HttpHeader, McpServerConfig, TransportType};
    use crate::server_connection::{McpClient, ServerNotification};
    use crate::server_transport::ConnectOptions;
    use crate::test_support::{FakeServer, RecordedRequest, Reply};
    use crate::tool_operations::CallOptions;

    type BoxedHandler = Box<dyn Fn(&RecordedRequest) -> Reply + Send + Sync>;

    fn initialize_reply(request: &RecordedRequest, version: &str, capabilities: &str) -> Reply {
        Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"protocolVersion":"{version}","capabilities":{capabilities},"serverInfo":{{"name":"remote","version":"2"}}}}}}"#,
            request.request_id().unwrap()
        ))
    }

    fn tools_reply(request: &RecordedRequest, tool: &str) -> Reply {
        Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"tools":[{{"name":"{tool}","inputSchema":{{"type":"object"}}}}]}}}}"#,
            request.request_id().unwrap()
        ))
    }

    fn final_event(request: &RecordedRequest, text: &str) -> String {
        format!(
            "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}\n\n",
            request.request_id().unwrap()
        )
    }

    fn remote(url: &str) -> McpServerConfig {
        McpServerConfig {
            headers: vec![HttpHeader {
                name: "X-Workspace".to_owned(),
                value: "one".to_owned(),
            }],
            startup_timeout_ms: 5_000,
            ..McpServerConfig::remote("remote", TransportType::Http, url)
        }
    }

    fn session_handler(version: &'static str) -> impl Fn(&RecordedRequest) -> Reply {
        move |request| match (request.method.as_str(), request.method_name().as_deref()) {
            ("POST", Some("initialize")) => {
                initialize_reply(request, version, "{}").header("Mcp-Session-Id", "session-1")
            }
            ("POST", Some("notifications/initialized" | "notifications/cancelled")) => {
                Reply::status(202)
            }
            ("POST", Some("tools/list")) => tools_reply(request, "remote_tool"),
            ("POST", Some("tools/call")) => {
                let progress = format!(
                    "data: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{\"progressToken\":{},\"progress\":1}}}}\n\n",
                    request.request_id().unwrap()
                );
                let done = final_event(request, "remote result");
                Reply::sse(&[": keepalive\n\n", &progress, &done])
            }
            ("DELETE", _) => Reply::status(204),
            _ => Reply::status(500),
        }
    }

    #[tokio::test]
    async fn streamable_http_initializes_a_session_and_calls_tools_over_sse() {
        let server = FakeServer::start(session_handler("2025-11-25")).await;
        let client = McpClient::connect(&remote(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        assert_eq!(client.server_info().protocol_version, "2025-11-25");
        assert_eq!(client.server_info().name.as_deref(), Some("remote"));
        assert_eq!(client.tool_catalog().tools[0].name, "remote_tool");
        let progress = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&progress);
        let outcome = client
            .call_tool(
                "remote_tool",
                r#"{"q": 1}"#,
                CallOptions {
                    progress: Some(Arc::new(move |_| {
                        counter.fetch_add(1, Ordering::Relaxed);
                    })),
                    ..CallOptions::default()
                },
                Instant::now() + Duration::from_secs(10),
            )
            .await
            .unwrap();
        let ToolCallOutcome::Complete(result) = outcome else {
            panic!("expected a complete result");
        };
        assert_eq!(
            result.content,
            vec![ToolContent::Text {
                text: "remote result".to_owned()
            }]
        );
        assert_eq!(progress.load(Ordering::Relaxed), 1);
        client.shutdown(ShutdownMode::Graceful).await;

        let requests = server.requests();
        let initialize = &requests[0];
        assert_eq!(initialize.method, "POST");
        assert_eq!(initialize.path, "/mcp");
        assert_eq!(initialize.header("accept"), Some(ACCEPT_JSON_AND_EVENTS));
        assert_eq!(initialize.header("content-type"), Some("application/json"));
        assert_eq!(initialize.header("x-workspace"), Some("one"));
        assert_eq!(initialize.header("mcp-protocol-version"), None);
        assert_eq!(initialize.header("mcp-session-id"), None);
        assert!(
            initialize
                .body
                .contains("\"protocolVersion\":\"2025-11-25\"")
        );
        let initialized = &requests[1];
        assert_eq!(initialized.body, HTTP_INITIALIZED_NOTIFICATION);
        for later in &requests[1..] {
            assert_eq!(later.header("mcp-protocol-version"), Some("2025-11-25"));
            assert_eq!(later.header("mcp-session-id"), Some("session-1"));
        }
        let teardown = requests.last().unwrap();
        assert_eq!(teardown.method, "DELETE");
        assert_eq!(teardown.header("accept"), Some(ACCEPT_EVENTS));
    }

    #[tokio::test]
    async fn streamable_http_2025_03_26_omits_the_protocol_header() {
        let server = FakeServer::start(session_handler("2025-03-26")).await;
        let client = McpClient::connect(&remote(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        assert_eq!(client.server_info().protocol_version, "2025-03-26");
        client.shutdown(ShutdownMode::ProcessExit).await;
        let requests = server.requests();
        assert!(
            requests
                .iter()
                .all(|request| request.header("mcp-protocol-version").is_none())
        );
        assert!(requests.iter().all(|request| request.method != "DELETE"));
    }

    #[tokio::test]
    async fn streamable_http_startup_failures_keep_upstream_error_names() {
        let cases: Vec<(BoxedHandler, McpError)> = vec![
            (
                Box::new(|request| initialize_reply(request, "2024-11-05", "{}")),
                McpError::McpUnsupportedProtocolVersion,
            ),
            (
                Box::new(|_| Reply::status(401).header("WWW-Authenticate", "Bearer")),
                McpError::McpAuthenticationRequired,
            ),
            (
                Box::new(|_| Reply::status(403).header("WWW-Authenticate", "Bearer scope=\"x\"")),
                McpError::McpAuthenticationRequired,
            ),
            (
                Box::new(|_| Reply::status(403)),
                McpError::UnexpectedHttpStatus,
            ),
            (
                Box::new(|_| Reply::status(307).header("Location", "https://example.test/mcp")),
                McpError::RedirectNotAllowed,
            ),
            (
                Box::new(|request| {
                    initialize_reply(request, "2025-11-25", "{}").header("Content-Encoding", "gzip")
                }),
                McpError::UnsupportedContentEncoding,
            ),
            (
                Box::new(|_| Reply::status(200).header("Content-Type", "text/plain")),
                McpError::UnsupportedContentType,
            ),
            (
                Box::new(|request| {
                    initialize_reply(request, "2025-11-25", "{}").header("Mcp-Session-Id", "bad id")
                }),
                McpError::InvalidMcpSessionId,
            ),
        ];
        for (handler, expected) in cases {
            let server = FakeServer::start(handler).await;
            let failure = McpClient::connect(&remote(&server.url), &ConnectOptions::default())
                .await
                .err()
                .unwrap();
            assert_eq!(failure.error, expected);
        }
    }

    #[tokio::test]
    async fn expired_sessions_fail_the_call_and_skip_the_delete() {
        let server = FakeServer::start(|request| match request.method_name().as_deref() {
            Some("tools/call") => Reply::status(404),
            _ => session_handler("2025-11-25")(request),
        })
        .await;
        let client = McpClient::connect(&remote(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        assert_eq!(
            client
                .call_tool(
                    "remote_tool",
                    "{}",
                    CallOptions::default(),
                    Instant::now() + Duration::from_secs(10)
                )
                .await,
            Err(McpError::McpSessionExpired)
        );
        client.shutdown(ShutdownMode::Graceful).await;
        assert!(
            server
                .requests()
                .iter()
                .all(|request| request.method != "DELETE")
        );
    }

    #[tokio::test]
    async fn sse_responses_resume_with_the_last_event_id() {
        let server = FakeServer::start(|request| match (request.method.as_str(), request.method_name().as_deref()) {
            ("POST", Some("tools/call")) => Reply::sse(&[
                "id: event-1\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n\n",
            ]),
            ("GET", _) => {
                let id = 3;
                Reply::sse(&[&format!(
                    "data: {{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"content\":[]}}}}\n\n"
                )])
            }
            _ => session_handler("2025-11-25")(request),
        })
        .await;
        let client = McpClient::connect(&remote(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        let outcome = client
            .call_tool(
                "remote_tool",
                "{}",
                CallOptions::default(),
                Instant::now() + Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ToolCallOutcome::Complete(_)));
        let resume = server
            .requests()
            .into_iter()
            .find(|request| request.method == "GET")
            .unwrap();
        assert_eq!(resume.header("last-event-id"), Some("event-1"));
        assert_eq!(resume.header("accept"), Some(ACCEPT_EVENTS));
        client.shutdown(ShutdownMode::ProcessExit).await;
    }

    #[tokio::test]
    async fn sse_streams_that_close_without_a_final_response_fail() {
        let server = FakeServer::start(|request| match request.method_name().as_deref() {
            Some("tools/call") => Reply::sse(&[": nothing\n\n"]),
            _ => session_handler("2025-06-18")(request),
        })
        .await;
        let client = McpClient::connect(&remote(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        assert_eq!(
            client
                .call_tool(
                    "remote_tool",
                    "{}",
                    CallOptions::default(),
                    Instant::now() + Duration::from_secs(10)
                )
                .await,
            Err(McpError::MissingFinalResponse)
        );
        client.shutdown(ShutdownMode::ProcessExit).await;
    }

    #[tokio::test]
    async fn server_requests_during_tool_calls_are_answered_with_method_not_found() {
        let server = FakeServer::start(|request| match (request.method.as_str(), request.method_name().as_deref()) {
            ("POST", Some("tools/call")) => {
                let done = final_event(request, "done");
                Reply::sse(&[
                    "data: {\"jsonrpc\":\"2.0\",\"id\":\"s-1\",\"method\":\"sampling/createMessage\",\"params\":{}}\n\n",
                    &done,
                ])
            }
            ("POST", None) => Reply::status(202),
            _ => session_handler("2025-11-25")(request),
        })
        .await;
        let client = McpClient::connect(&remote(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        client
            .call_tool(
                "remote_tool",
                "{}",
                CallOptions::default(),
                Instant::now() + Duration::from_secs(10),
            )
            .await
            .unwrap();
        let answer = server
            .requests()
            .into_iter()
            .find(|request| request.body.contains("\"error\""))
            .unwrap();
        assert_eq!(
            answer.body,
            "{\"jsonrpc\":\"2.0\",\"id\":\"s-1\",\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}"
        );
        client.shutdown(ShutdownMode::ProcessExit).await;
    }

    #[tokio::test]
    async fn the_notification_stream_invalidates_the_tool_catalog() {
        let listed = Arc::new(AtomicUsize::new(0));
        let lists = Arc::clone(&listed);
        let server = FakeServer::start(move |request| match (request.method.as_str(), request.method_name().as_deref()) {
            ("POST", Some("initialize")) => {
                initialize_reply(request, "2025-11-25", r#"{"tools":{"listChanged":true}}"#)
            }
            ("POST", Some("tools/list")) => {
                let tool = if lists.fetch_add(1, Ordering::Relaxed) == 0 { "before" } else { "after" };
                tools_reply(request, tool)
            }
            ("GET", _) => Reply::sse(&[
                "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n",
            ])
            .held_open(),
            _ => session_handler("2025-11-25")(request),
        })
        .await;
        let client = McpClient::connect(&remote(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        assert_eq!(client.tool_catalog().tools[0].name, "before");
        assert!(server.wait_for(|request| request.method == "GET").await);
        let mut refreshed = client
            .refresh_tools(Instant::now() + Duration::from_secs(5))
            .await
            .catalog;
        for _ in 0..100 {
            if refreshed.tools[0].name == "after" {
                break;
            }
            sleep(Duration::from_millis(20)).await;
            refreshed = client
                .refresh_tools(Instant::now() + Duration::from_secs(5))
                .await
                .catalog;
        }
        assert_eq!(refreshed.tools[0].name, "after");
        client.shutdown(ShutdownMode::ProcessExit).await;
    }

    #[tokio::test]
    async fn notifications_on_a_tool_call_stream_reach_the_client() {
        let listed = Arc::new(AtomicUsize::new(0));
        let lists = Arc::clone(&listed);
        let server = FakeServer::start(move |request| match (request.method.as_str(), request.method_name().as_deref()) {
            ("POST", Some("initialize")) => {
                initialize_reply(request, "2025-11-25", r#"{"tools":{"listChanged":true}}"#)
            }
            ("POST", Some("tools/list")) => {
                let tool = if lists.fetch_add(1, Ordering::Relaxed) == 0 { "before" } else { "after" };
                tools_reply(request, tool)
            }
            ("POST", Some("tools/call")) => {
                let done = final_event(request, "done");
                Reply::sse(&[
                    "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n",
                    &done,
                ])
            }
            ("GET", _) => Reply::status(405),
            _ => session_handler("2025-11-25")(request),
        })
        .await;
        let client = McpClient::connect(&remote(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        let outcome = client
            .call_tool(
                "before",
                "{}",
                CallOptions::default(),
                Instant::now() + Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ToolCallOutcome::Complete(_)));
        assert_eq!(
            client.next_notification().await,
            Some(ServerNotification::ToolsListChanged)
        );
        assert_eq!(
            client
                .refresh_tools(Instant::now() + Duration::from_secs(5))
                .await
                .catalog
                .tools[0]
                .name,
            "after"
        );
        client.shutdown(ShutdownMode::ProcessExit).await;
    }

    #[tokio::test]
    async fn an_interrupted_tool_call_stream_resumes_from_its_last_event() {
        let server = FakeServer::start(|request| match (request.method.as_str(), request.method_name().as_deref()) {
            ("POST", Some("tools/call")) => Reply::sse(&[
                "id: event-1\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n\n",
                "id: event-2\ndata: {\"jsonrpc\":\"2.0\",",
            ])
            .interrupted(),
            ("GET", _) => Reply::sse(&[
                "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[]}}\n\n",
            ]),
            _ => session_handler("2025-11-25")(request),
        })
        .await;
        let client = McpClient::connect(&remote(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        let outcome = client
            .call_tool(
                "remote_tool",
                "{}",
                CallOptions::default(),
                Instant::now() + Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ToolCallOutcome::Complete(_)));
        let requests = server.requests();
        let resume = requests
            .iter()
            .find(|request| request.method == "GET")
            .unwrap();
        assert_eq!(resume.header("last-event-id"), Some("event-1"));
        let calls = requests
            .iter()
            .filter(|request| request.method_name().as_deref() == Some("tools/call"))
            .count();
        assert_eq!(calls, 1);
        client.shutdown(ShutdownMode::ProcessExit).await;
    }

    #[tokio::test]
    async fn an_interrupted_tool_call_stream_without_an_event_id_fails() {
        let server = FakeServer::start(|request| match request.method_name().as_deref() {
            Some("tools/call") => Reply::sse(&[
                "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n\n",
            ])
            .interrupted(),
            _ => session_handler("2025-11-25")(request),
        })
        .await;
        let client = McpClient::connect(&remote(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        assert!(
            client
                .call_tool(
                    "remote_tool",
                    "{}",
                    CallOptions::default(),
                    Instant::now() + Duration::from_secs(10)
                )
                .await
                .is_err()
        );
        assert!(
            server
                .requests()
                .iter()
                .all(|request| request.method != "GET")
        );
        client.shutdown(ShutdownMode::ProcessExit).await;
    }

    #[tokio::test]
    async fn an_interrupted_notification_stream_resumes_from_its_last_event() {
        let gets = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&gets);
        let server = FakeServer::start(move |request| match (request.method.as_str(), request.method_name().as_deref()) {
            ("POST", Some("initialize")) => {
                initialize_reply(request, "2025-11-25", r#"{"tools":{"listChanged":true}}"#)
            }
            ("GET", _) if seen.fetch_add(1, Ordering::Relaxed) == 0 => Reply::sse(&[
                "id: notice-1\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n\n",
            ])
            .interrupted(),
            ("GET", _) => Reply::sse(&[]).held_open(),
            _ => session_handler("2025-11-25")(request),
        })
        .await;
        let client = McpClient::connect(&remote(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        assert!(
            server
                .wait_for(|request| request.method == "GET"
                    && request.header("last-event-id") == Some("notice-1"))
                .await
        );
        client.shutdown(ShutdownMode::ProcessExit).await;
    }

    #[tokio::test]
    async fn a_failed_notification_stream_keeps_its_cursor_for_the_retry() {
        let gets = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&gets);
        let server = FakeServer::start(move |request| match (request.method.as_str(), request.method_name().as_deref()) {
            ("POST", Some("initialize")) => {
                initialize_reply(request, "2025-11-25", r#"{"tools":{"listChanged":true}}"#)
            }
            ("GET", _) => match seen.fetch_add(1, Ordering::Relaxed) {
                0 => Reply::sse(&[
                    "id: notice-1\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n\n",
                ]),
                1 => Reply::status(503),
                _ => Reply::sse(&[]).held_open(),
            },
            _ => session_handler("2025-11-25")(request),
        })
        .await;
        let client = McpClient::connect(&remote(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        assert!(server.wait_for(|_| gets.load(Ordering::Relaxed) >= 3).await);
        let cursors: Vec<_> = server
            .requests()
            .iter()
            .filter(|request| request.method == "GET")
            .map(|request| request.header("last-event-id").map(str::to_owned))
            .collect();
        assert_eq!(
            cursors[..3],
            [
                None,
                Some("notice-1".to_owned()),
                Some("notice-1".to_owned())
            ]
        );
        client.shutdown(ShutdownMode::ProcessExit).await;
    }

    #[tokio::test]
    async fn an_unsupported_notification_stream_is_not_retried() {
        let server = FakeServer::start(|request| {
            match (request.method.as_str(), request.method_name().as_deref()) {
                ("POST", Some("initialize")) => {
                    initialize_reply(request, "2025-11-25", r#"{"tools":{"listChanged":true}}"#)
                }
                ("GET", _) => Reply::status(405),
                _ => session_handler("2025-11-25")(request),
            }
        })
        .await;
        let client = McpClient::connect(&remote(&server.url), &ConnectOptions::default())
            .await
            .unwrap();
        assert!(server.wait_for(|request| request.method == "GET").await);
        sleep(Duration::from_millis(300)).await;
        let gets = server
            .requests()
            .iter()
            .filter(|request| request.method == "GET")
            .count();
        assert_eq!(gets, 1);
        client.shutdown(ShutdownMode::ProcessExit).await;
    }

    #[test]
    fn legacy_streamable_http_version_policy_is_explicit() {
        let cases = [
            ("2025-11-25", HttpVersion::V2025_11_25, true, true),
            ("2025-06-18", HttpVersion::V2025_06_18, true, false),
            ("2025-03-26", HttpVersion::V2025_03_26, false, false),
        ];
        assert_eq!(HttpVersion::PREFERRED, HttpVersion::V2025_11_25);
        for (raw, version, sends_header, polling_close) in cases {
            assert_eq!(HttpVersion::parse(raw), Some(version));
            assert_eq!(version.sends_protocol_header(), sends_header);
            assert_eq!(version.allows_polling_close(), polling_close);
        }
        assert_eq!(HttpVersion::parse("2024-11-05"), None);
        assert_eq!(HttpVersion::parse("2026-07-28"), None);
    }

    #[test]
    fn legacy_streamable_http_polling_close_is_scoped_to_2025_11_25() {
        for version in [
            HttpVersion::V2025_11_25,
            HttpVersion::V2025_06_18,
            HttpVersion::V2025_03_26,
        ] {
            assert!(close_is_resumable(version, Some("event-1"), false));
        }
        assert!(close_is_resumable(HttpVersion::V2025_11_25, Some(""), true));
        assert!(!close_is_resumable(
            HttpVersion::V2025_06_18,
            Some(""),
            true
        ));
        assert!(!close_is_resumable(
            HttpVersion::V2025_03_26,
            Some(""),
            true
        ));
        assert!(!close_is_resumable(HttpVersion::V2025_11_25, None, false));
    }

    #[test]
    fn legacy_streamable_http_initialization_selects_only_committed_versions() {
        assert_eq!(
            initialized_version(
                &json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18"}})
            ),
            Ok(HttpVersion::V2025_06_18)
        );
        assert_eq!(
            initialized_version(
                &json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}})
            ),
            Err(McpError::McpUnsupportedProtocolVersion)
        );
    }

    #[test]
    fn legacy_streamable_http_validates_protocol_session_ids() {
        assert_eq!(validate_session_id("session-123"), Ok(()));
        for invalid in ["", "contains space", "contains\nnewline"] {
            assert_eq!(
                validate_session_id(invalid),
                Err(McpError::InvalidMcpSessionId)
            );
        }
        assert_eq!(
            validate_session_id(&"x".repeat(1025)),
            Err(McpError::InvalidMcpSessionId)
        );
    }

    #[test]
    fn legacy_streamable_http_classifies_final_progress_and_server_requests() {
        assert_eq!(
            classify_event(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#, 7, None),
            Ok(EventOutcome::Final)
        );
        assert_eq!(
            classify_event(
                r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{}}"#,
                7,
                None
            ),
            Ok(EventOutcome::Notification)
        );
        assert_eq!(
            classify_event(
                r#"{"jsonrpc":"2.0","id":7,"method":"roots/list","params":{}}"#,
                7,
                None
            ),
            Ok(EventOutcome::ServerRequest)
        );
        assert_eq!(
            classify_event(r#"{"jsonrpc":"2.0","id":8,"result":{}}"#, 7, None),
            Err(McpError::MismatchedResponseId)
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let sink: ProgressSink = Arc::new(move |update| lock(&sink_seen).push(update));
        assert_eq!(
            classify_event(
                r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":7,"progress":3,"message":"m"}}"#,
                7,
                Some(&sink)
            ),
            Ok(EventOutcome::ProgressNotification)
        );
        assert_eq!(lock(&seen).len(), 1);
        assert_eq!(
            classify_event(
                r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":8,"progress":3}}"#,
                7,
                Some(&sink)
            ),
            Ok(EventOutcome::Notification)
        );
        assert_eq!(classify_event("", 7, None), Ok(EventOutcome::Empty));
        assert_eq!(classify_event("{", 7, None), Err(McpError::InvalidSseEvent));
        assert_eq!(
            classify_event(r#"{"jsonrpc":"2.0","id":7,"id":8,"result":{}}"#, 7, None),
            Err(McpError::InvalidSseEvent)
        );
        assert_eq!(
            validate_final_response(r#"{"jsonrpc":"2.0","id":7,"result":{"a":1,"a":2}}"#, 7),
            Err(McpError::McpInvalidJson)
        );
        assert_eq!(
            validate_final_response(r#"{"jsonrpc":"2.0","id":7,"result":{"a":1}}"#, 7),
            Ok(())
        );
    }

    #[test]
    fn retry_delays_double_up_to_five_seconds() {
        assert_eq!(retry_delay(0), Duration::from_millis(100));
        assert_eq!(retry_delay(1), Duration::from_millis(200));
        assert_eq!(retry_delay(8), Duration::from_secs(5));
        assert_eq!(retry_delay(20), Duration::from_secs(5));
    }
}
