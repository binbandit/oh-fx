use std::error::Error;
use std::fmt::Write;
use std::future::Future;
use std::io;
use std::time::Duration;

use ofx_config::ResolvedConnection;
use ofx_contract::{
    BoxFuture, Completion, ModelProvider, ModelRequest, ProviderError, ProviderErrorKind,
    StreamEvent, StreamSink,
};
use ofx_http::{ClientError, ConnectionOptions, SseDecoder, SseError, build_connection_client};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, LOCATION, RETRY_AFTER};
use reqwest::{RequestBuilder, Response, StatusCode, Url};
use tokio_util::sync::CancellationToken;

use crate::chat_completions_protocol::{
    Limits, ProtocolError, Reducer, RequestOptions, build_request, mask_configured_secrets,
    redact_error_detail,
};
use crate::gateway_error_format::{
    format_http_error_message, format_http_recovery_diagnostic, sanitize_external_text,
};

const RESPONSE_HEAD_TIMEOUT: Duration = Duration::from_mins(2);
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_DISPLAYED_BODY_BYTES: usize = 4096;
const MAX_DETAIL_BYTES: usize = 512;
const EVENT_STREAM: &str = "text/event-stream";
const ERROR_BODY_LIMIT_NOTICE: &str = "Provider error response exceeded the local limit";
const RETRYABLE_CONNECT_ERRORS: [io::ErrorKind; 9] = [
    io::ErrorKind::ConnectionRefused,
    io::ErrorKind::ConnectionReset,
    io::ErrorKind::ConnectionAborted,
    io::ErrorKind::NotConnected,
    io::ErrorKind::TimedOut,
    io::ErrorKind::HostUnreachable,
    io::ErrorKind::NetworkUnreachable,
    io::ErrorKind::NetworkDown,
    io::ErrorKind::AddrNotAvailable,
];

pub struct ChatCompletionsProvider {
    client: reqwest::Client,
    chat_url: String,
    bearer_token: Option<String>,
    secrets: Vec<String>,
    options: RequestOptions,
}

impl ChatCompletionsProvider {
    pub fn new(connection: ResolvedConnection, user_agent: &str) -> Result<Self, ClientError> {
        let ResolvedConnection {
            chat_url,
            bearer_token,
            headers,
            secrets,
            tool_choice_mode,
            max_tokens_parameter,
            ca_file,
            proxy,
        } = connection;
        let client = build_connection_client(&ConnectionOptions {
            user_agent: user_agent.to_owned(),
            default_headers: headers,
            ca_file,
            proxy,
            follow_redirects: false,
        })?;
        Ok(Self {
            client,
            chat_url,
            bearer_token,
            secrets,
            options: RequestOptions {
                tool_choice_mode,
                max_tokens_parameter,
            },
        })
    }

    async fn complete(
        &self,
        request: &ModelRequest<'_>,
        sink: &mut dyn StreamSink,
        cancel: &CancellationToken,
    ) -> Result<Completion, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        let prepared = build_request(request, self.options).map_err(protocol_failure)?;
        let mut builder = self
            .client
            .post(&self.chat_url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, EVENT_STREAM)
            .body(prepared.body);
        if let Some(token) = &self.bearer_token {
            builder = builder.bearer_auth(token);
        }
        let mut response = match send(builder, cancel).await {
            Ok(response) => response,
            Err(SendFailure::Cancelled) => return Err(ProviderError::cancelled()),
            Err(SendFailure::Timeout) => {
                return Err(ProviderError::new(ProviderErrorKind::Timeout, "Timeout"));
            }
            Err(SendFailure::Transport(error)) => {
                return Err(transport_failure(&error, &self.secrets));
            }
        };
        if response.status() != StatusCode::OK {
            return Err(http_failure(response, cancel, &self.secrets, Some(&self.chat_url)).await);
        }
        if let Some(media_type) = unexpected_media_type(response.headers()) {
            return Err(self.unexpected_body(&media_type, response, cancel).await);
        }
        let limits = Limits::default();
        let mut reducer = Reducer::new(prepared.selection, limits);
        let stream = Stream {
            limits,
            secrets: &self.secrets,
        };
        stream
            .consume(&mut response, &mut reducer, sink, cancel)
            .await
    }

    async fn unexpected_body(
        &self,
        media_type: &str,
        mut response: Response,
        cancel: &CancellationToken,
    ) -> ProviderError {
        let body = match read_body(&mut response, cancel).await {
            Ok(body) => body,
            Err(failure) => return failure,
        };
        if let Some(error) = body.as_deref().and_then(provider_error_body) {
            let detail = redact_error_detail(error.as_bytes(), &self.secrets);
            return ProviderError::new(ProviderErrorKind::ProviderError, "ProviderError")
                .with_detail(sanitized(
                    format!("provider error: {detail}"),
                    &self.secrets,
                ));
        }
        let size = body.map_or_else(
            || format!("more than {MAX_ERROR_BODY_BYTES} bytes"),
            |body| format!("{} bytes", body.len()),
        );
        let detail = format!("expected {EVENT_STREAM} but the provider sent {media_type} ({size})");
        ProviderError::new(ProviderErrorKind::Protocol, "UnexpectedContentType")
            .with_detail(sanitized(detail, &self.secrets))
    }
}

pub(crate) fn sanitized(text: String, secrets: &[String]) -> String {
    sanitize_external_text(&mask_configured_secrets(text, secrets), MAX_DETAIL_BYTES)
}

pub(crate) fn transport_failure(error: &reqwest::Error, secrets: &[String]) -> ProviderError {
    let (kind, code) = if error.is_connect() && is_connectivity_loss(error) {
        (ProviderErrorKind::ConnectivityLost, "ConnectionFailed")
    } else if error.is_connect() {
        (ProviderErrorKind::ConnectionFailed, "ConnectionFailed")
    } else if error.is_timeout() {
        (ProviderErrorKind::Timeout, "Timeout")
    } else {
        (ProviderErrorKind::TransportInterrupted, "RequestFailed")
    };
    ProviderError::new(kind, code).with_detail(sanitized(error_chain(error), secrets))
}

pub(crate) async fn http_failure(
    mut response: Response,
    cancel: &CancellationToken,
    secrets: &[String],
    redirect_base: Option<&str>,
) -> ProviderError {
    let status = response.status();
    let retry_after = retry_after(response.headers());
    let detail = match redirect_base.filter(|_| status.is_redirection()) {
        Some(base) => redirect_notice(response.headers(), base),
        None => match read_body(&mut response, cancel).await {
            Ok(Some(body)) => redact_error_detail(&body, secrets),
            Ok(None) => ERROR_BODY_LIMIT_NOTICE.to_owned(),
            Err(failure) => return failure,
        },
    };
    let kind = failure_kind(status);
    let displayed = detail.trim_matches([' ', '\t', '\r', '\n']);
    let displayed = &displayed[..displayed.floor_char_boundary(MAX_DISPLAYED_BODY_BYTES)];
    let mut error = ProviderError::new(kind, failure_code(kind))
        .with_detail(format_http_error_message(status.as_u16(), displayed));
    error.status = Some(status.as_u16());
    error.diagnostic = Some(format_http_recovery_diagnostic(status.as_u16(), &detail));
    error.retry_after = retry_after;
    error
}

impl ModelProvider for ChatCompletionsProvider {
    fn stream<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        sink: &'a mut dyn StreamSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        Box::pin(self.complete(request, sink, cancel))
    }
}

pub(crate) enum SendFailure {
    Cancelled,
    Timeout,
    Transport(reqwest::Error),
}

pub(crate) async fn send(
    builder: RequestBuilder,
    cancel: &CancellationToken,
) -> Result<Response, SendFailure> {
    let outcome = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(SendFailure::Cancelled),
        outcome = tokio::time::timeout(RESPONSE_HEAD_TIMEOUT, builder.send()) => outcome,
    };
    match outcome {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) => Err(SendFailure::Transport(error)),
        Err(_) => Err(SendFailure::Timeout),
    }
}

async fn read_body(
    response: &mut Response,
    cancel: &CancellationToken,
) -> Result<Option<Vec<u8>>, ProviderError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ProviderError::cancelled()),
        body = tokio::time::timeout(ERROR_BODY_TIMEOUT, read_bounded_body(response)) => {
            body.map_err(|_| ProviderError::new(ProviderErrorKind::Timeout, "Timeout"))
        }
    }
}

async fn read_bounded_body(response: &mut Response) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if body.len() + chunk.len() > MAX_ERROR_BODY_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim_matches([' ', '\t']).parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn redirect_notice(headers: &HeaderMap, chat_url: &str) -> String {
    let target = headers
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|location| Url::parse(chat_url).ok()?.join(location).ok())
        .and_then(|url| {
            let host = url.host_str()?.to_owned();
            Some(match url.port() {
                Some(port) => format!("{}://{host}:{port}", url.scheme()),
                None => format!("{}://{host}", url.scheme()),
            })
        })
        .unwrap_or_else(|| "an unreadable location".to_owned());
    format!(
        "redirect to {target} was not followed; base_url must point at the gateway API itself, not at a sign-in page or a proxy that redirects"
    )
}

fn unexpected_media_type(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(CONTENT_TYPE)?;
    let media_type = value
        .to_str()
        .ok()
        .map(|text| {
            text.split(';')
                .next()
                .unwrap_or(text)
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    (media_type != EVENT_STREAM).then_some(media_type)
}

fn provider_error_body(body: &[u8]) -> Option<String> {
    let serde_json::Value::Object(root) = serde_json::from_slice(body).ok()? else {
        return None;
    };
    if let Some(error) = root.get("error").filter(|error| !error.is_null()) {
        return Some(error.to_string());
    }
    (root.get("object").and_then(serde_json::Value::as_str) == Some("error"))
        .then(|| serde_json::Value::Object(root).to_string())
}

fn is_connectivity_loss(error: &reqwest::Error) -> bool {
    let mut source = error.source();
    while let Some(cause) = source {
        if cause.to_string() == "dns error" {
            return true;
        }
        if let Some(io_error) = cause.downcast_ref::<io::Error>()
            && RETRYABLE_CONNECT_ERRORS.contains(&io_error.kind())
        {
            return true;
        }
        source = cause.source();
    }
    false
}

fn error_chain(error: &dyn Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let _ = write!(text, ": {cause}");
        source = cause.source();
    }
    text
}

fn failure_kind(status: StatusCode) -> ProviderErrorKind {
    match status.as_u16() {
        400 => ProviderErrorKind::InvalidRequest,
        401 => ProviderErrorKind::Unauthorized,
        403 => ProviderErrorKind::Forbidden,
        413 => ProviderErrorKind::RequestTooLarge,
        429 => ProviderErrorKind::RateLimited,
        500 => ProviderErrorKind::ServerError,
        502 => ProviderErrorKind::BadGateway,
        503 => ProviderErrorKind::Unavailable,
        504 => ProviderErrorKind::GatewayTimeout,
        _ => ProviderErrorKind::ProviderError,
    }
}

const fn failure_code(kind: ProviderErrorKind) -> &'static str {
    match kind {
        ProviderErrorKind::InvalidRequest => "invalid_request",
        ProviderErrorKind::Unauthorized => "unauthorized",
        ProviderErrorKind::Forbidden => "forbidden",
        ProviderErrorKind::RequestTooLarge => "request_too_large",
        ProviderErrorKind::RateLimited => "rate_limited",
        ProviderErrorKind::ServerError => "server_error",
        ProviderErrorKind::BadGateway => "bad_gateway",
        ProviderErrorKind::Unavailable => "unavailable",
        ProviderErrorKind::GatewayTimeout => "gateway_timeout",
        _ => "provider_error",
    }
}

fn protocol_failure(error: ProtocolError) -> ProviderError {
    let kind = match error {
        ProtocolError::Cancelled => ProviderErrorKind::Cancelled,
        ProtocolError::ProviderError => ProviderErrorKind::ProviderError,
        _ => ProviderErrorKind::Protocol,
    };
    ProviderError::new(kind, error.to_string())
}

pub(crate) trait ChunkSource {
    fn next_chunk(
        &mut self,
    ) -> impl Future<Output = Result<Option<impl AsRef<[u8]> + Send>, String>> + Send;
}

impl ChunkSource for Response {
    async fn next_chunk(&mut self) -> Result<Option<impl AsRef<[u8]> + Send>, String> {
        self.chunk().await.map_err(|error| error_chain(&error))
    }
}

pub(crate) struct Stream<'a> {
    pub(crate) limits: Limits,
    pub(crate) secrets: &'a [String],
}

impl Stream<'_> {
    pub(crate) async fn consume<S: ChunkSource + Send>(
        &self,
        source: &mut S,
        reducer: &mut Reducer,
        sink: &mut dyn StreamSink,
        cancel: &CancellationToken,
    ) -> Result<Completion, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        let limits = self.limits;
        let mut decoder = SseDecoder::new(limits.event_bytes);
        let mut events = 0;
        let mut received = 0;
        loop {
            let remaining = limits
                .total_wire_bytes
                .saturating_sub(decoder.total_bytes());
            let event_limited = limits.event_bytes < remaining;
            decoder.set_total_limit(Some(
                decoder.total_bytes() + limits.event_bytes.min(remaining),
            ));
            match decoder.next_event() {
                Ok(Some(data)) => {
                    events += 1;
                    let deltas = reducer
                        .accept(data, cancel.is_cancelled())
                        .map_err(|error| self.event_failure(error, reducer, events, data.len()))?;
                    for text in deltas.reasoning {
                        if cancel.is_cancelled() {
                            return Err(ProviderError::cancelled());
                        }
                        sink.emit(StreamEvent::ReasoningDelta { text });
                    }
                    if let Some(text) = deltas.content {
                        if cancel.is_cancelled() {
                            return Err(ProviderError::cancelled());
                        }
                        sink.emit(StreamEvent::TextDelta { text });
                    }
                    if reducer.is_done() {
                        return self.finish(reducer, cancel, received);
                    }
                    continue;
                }
                Ok(None) => {}
                Err(SseError::StreamTooLarge) if !event_limited => {
                    return Err(protocol_failure(ProtocolError::StreamTooLarge));
                }
                Err(_) => return Err(protocol_failure(ProtocolError::EventTooLarge)),
            }
            let chunk = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(ProviderError::cancelled()),
                chunk = source.next_chunk() => chunk,
            };
            match chunk {
                Ok(Some(bytes)) => {
                    let bytes = bytes.as_ref();
                    received += bytes.len();
                    decoder.push(bytes);
                }
                Ok(None) => {
                    reducer
                        .end_of_stream()
                        .map_err(|error| self.completion_failure(error, reducer, received))?;
                    return self.finish(reducer, cancel, received);
                }
                Err(detail) => {
                    return Err(ProviderError::new(
                        ProviderErrorKind::TransportInterrupted,
                        "ReadFailed",
                    )
                    .with_detail(sanitized(detail, self.secrets)));
                }
            }
        }
    }

    fn finish(
        &self,
        reducer: &mut Reducer,
        cancel: &CancellationToken,
        received: usize,
    ) -> Result<Completion, ProviderError> {
        reducer
            .finish(cancel.is_cancelled())
            .map_err(|error| self.completion_failure(error, reducer, received))
    }

    fn event_failure(
        &self,
        error: ProtocolError,
        reducer: &mut Reducer,
        index: usize,
        size: usize,
    ) -> ProviderError {
        let failure = protocol_failure(error);
        if matches!(
            error,
            ProtocolError::Cancelled | ProtocolError::StreamClosed
        ) {
            return failure;
        }
        let detail = match reducer.take_failure_detail() {
            Some(error) => format!(
                "provider error: {}",
                redact_error_detail(error.as_bytes(), self.secrets)
            ),
            None => format!("stream event {index} ({size} bytes) was rejected"),
        };
        failure.with_detail(sanitized(detail, self.secrets))
    }

    fn completion_failure(
        &self,
        error: ProtocolError,
        reducer: &mut Reducer,
        received: usize,
    ) -> ProviderError {
        let failure = protocol_failure(error);
        let detail = match error {
            ProtocolError::IncompleteStream => format!(
                "the stream ended before a finish_reason after {} events and {received} bytes",
                reducer.event_count()
            ),
            _ => match reducer.take_failure_detail() {
                Some(detail) => detail,
                None => return failure,
            },
        };
        failure.with_detail(sanitized(detail, self.secrets))
    }
}

#[cfg(test)]
mod tests;
