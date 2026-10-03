use std::collections::VecDeque;
use std::fmt::Write;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::Acceptor;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_rustls::LazyConfigAcceptor;

const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const SERVER_KEY_PEM: &str = include_str!("tls/server.key");
const WEB_SERVER_KEY_PEM: &str = include_str!("tls/web-server.key");
const WEB_SERVER_CERTIFICATE_PEM: &str = include_str!("tls/web-server.pem");
pub const TEST_CA_PEM: &str = include_str!("tls/ca.pem");
pub const OTHER_CA_PEM: &str = include_str!("tls/other-ca.pem");
pub const WEB_CA_PEM: &str = include_str!("tls/web-ca.pem");
pub const TEST_SERVER_CERTIFICATE_PEM: &str = include_str!("tls/server.pem");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Stream {
        chunks: Vec<Vec<u8>>,
        hold_open: bool,
        flushed: Option<Gate>,
    },
    Status {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
        hold_open: bool,
        delay: Duration,
    },
    CutOff {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    },
    Disconnect,
    Gated {
        gate: Gate,
        reply: Box<Reply>,
    },
    Raw(Vec<u8>),
}

#[derive(Debug, Clone, Default)]
pub struct Gate(Arc<watch::Sender<bool>>);

impl Gate {
    pub fn is_open(&self) -> bool {
        *self.0.borrow()
    }

    pub fn open(&self) {
        self.0.send_replace(true);
    }

    async fn passed(&self, signal: &mut watch::Receiver<bool>) -> bool {
        let mut opened = self.0.subscribe();
        tokio::select! {
            _ = signal.changed() => false,
            passed = opened.wait_for(|open| *open) => passed.is_ok(),
        }
    }
}

impl PartialEq for Gate {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Gate {}

impl Reply {
    #[must_use]
    pub fn after(self, gate: &Gate) -> Self {
        Self::Gated {
            gate: gate.clone(),
            reply: Box::new(self),
        }
    }

    pub fn sse<S: AsRef<str>>(events: &[S]) -> Self {
        Self::Stream {
            chunks: sse_chunks(events),
            hold_open: false,
            flushed: None,
        }
    }

    pub fn held_sse<S: AsRef<str>>(events: &[S]) -> Self {
        Self::Stream {
            chunks: sse_chunks(events),
            hold_open: true,
            flushed: None,
        }
    }

    pub fn held_sse_with_flush<S: AsRef<str>>(events: &[S], flushed: &Gate) -> Self {
        Self::Stream {
            chunks: sse_chunks(events),
            hold_open: true,
            flushed: Some(flushed.clone()),
        }
    }

    pub fn status(status: u16, body: impl Into<String>) -> Self {
        Self::status_with_headers(status, &[], body)
    }

    pub fn status_with_headers(
        status: u16,
        headers: &[(&str, &str)],
        body: impl Into<String>,
    ) -> Self {
        Self::Status {
            status,
            headers: owned_headers(headers),
            body: body.into(),
            hold_open: false,
            delay: Duration::ZERO,
        }
    }

    pub fn held_status_with_headers(
        status: u16,
        headers: &[(&str, &str)],
        body: impl Into<String>,
    ) -> Self {
        Self::Status {
            status,
            headers: owned_headers(headers),
            body: body.into(),
            hold_open: true,
            delay: Duration::ZERO,
        }
    }

    pub fn delayed_status(status: u16, body: impl Into<String>, delay: Duration) -> Self {
        Self::Status {
            status,
            headers: Vec::new(),
            body: body.into(),
            hold_open: false,
            delay,
        }
    }

    pub fn cut_off(status: u16, headers: &[(&str, &str)], body: impl Into<String>) -> Self {
        Self::CutOff {
            status,
            headers: owned_headers(headers),
            body: body.into(),
        }
    }
}

fn owned_headers(headers: &[(&str, &str)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn sse_chunks<S: AsRef<str>>(events: &[S]) -> Vec<Vec<u8>> {
    events
        .iter()
        .map(|event| format!("data: {}\n\n", event.as_ref()).into_bytes())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

#[derive(Debug, Default)]
struct State {
    replies: VecDeque<Reply>,
    requests: Vec<RecordedRequest>,
    offered_protocols: Vec<Vec<String>>,
}

#[derive(Debug)]
pub struct FakeServer {
    address: SocketAddr,
    scheme: &'static str,
    state: Arc<Mutex<State>>,
    shutdown: watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeServer {
    pub fn start(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self::serve_on_loopback(replies, None)
    }

    pub fn start_tls(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self::serve_on_loopback(
            replies,
            Some(server_config(TEST_SERVER_CERTIFICATE_PEM, SERVER_KEY_PEM)),
        )
    }

    pub fn start_web_tls(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self::serve_on_loopback(
            replies,
            Some(server_config(
                WEB_SERVER_CERTIFICATE_PEM,
                WEB_SERVER_KEY_PEM,
            )),
        )
    }

    fn serve_on_loopback(
        replies: impl IntoIterator<Item = Reply>,
        tls: Option<Arc<ServerConfig>>,
    ) -> Self {
        let scheme = if tls.is_some() { "https" } else { "http" };
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        listener
            .set_nonblocking(true)
            .expect("make the listener non-blocking");
        let address = listener.local_addr().expect("read the bound address");
        let state = Arc::new(Mutex::new(State {
            replies: replies.into_iter().collect(),
            ..State::default()
        }));
        let (shutdown, signal) = watch::channel(false);
        let served_state = Arc::clone(&state);
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build the fake server runtime");
            runtime.block_on(serve(listener, tls, served_state, signal));
        });
        Self {
            address,
            scheme,
            state,
            shutdown,
            thread: Some(thread),
        }
    }

    pub fn base_url(&self) -> String {
        format!("{}://{}/v1", self.scheme, self.address)
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.state).requests.clone()
    }

    pub fn offered_protocols(&self) -> Vec<Vec<String>> {
        lock(&self.state).offered_protocols.clone()
    }
}

fn server_config(certificate_pem: &str, key_pem: &str) -> Arc<ServerConfig> {
    let certificates = CertificateDer::pem_slice_iter(certificate_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .expect("parse the test server certificate");
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).expect("parse the test server key");
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("choose TLS versions")
            .with_no_client_auth()
            .with_single_cert(certificates, key)
            .expect("configure the test server certificate");
    Arc::new(config)
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn serve(
    listener: StdTcpListener,
    tls: Option<Arc<ServerConfig>>,
    state: Arc<Mutex<State>>,
    mut signal: watch::Receiver<bool>,
) {
    let Ok(listener) = TcpListener::from_std(listener) else {
        return;
    };
    loop {
        tokio::select! {
            _ = signal.changed() => return,
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    let state = Arc::clone(&state);
                    match &tls {
                        Some(config) => {
                            tokio::spawn(handle_tls(stream, Arc::clone(config), state, signal.clone()));
                        }
                        None => {
                            tokio::spawn(handle(stream, state, signal.clone()));
                        }
                    }
                }
            }
        }
    }
}

async fn handle_tls(
    stream: TcpStream,
    config: Arc<ServerConfig>,
    state: Arc<Mutex<State>>,
    signal: watch::Receiver<bool>,
) {
    let Ok(start) = LazyConfigAcceptor::new(Acceptor::default(), stream).await else {
        return;
    };
    let offered = start
        .client_hello()
        .alpn()
        .map(|protocols| {
            protocols
                .map(|protocol| String::from_utf8_lossy(protocol).into_owned())
                .collect()
        })
        .unwrap_or_default();
    lock(&state).offered_protocols.push(offered);
    if let Ok(stream) = start.into_stream(config).await {
        handle(stream, state, signal).await;
    }
}

async fn handle<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    state: Arc<Mutex<State>>,
    mut signal: watch::Receiver<bool>,
) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let reply = {
        let mut state = lock(&state);
        state.requests.push(request);
        state.replies.pop_front()
    };
    let mut reply = reply.unwrap_or_else(|| Reply::status(500, "no scripted reply"));
    while let Reply::Gated { gate, reply: held } = reply {
        if !gate.passed(&mut signal).await {
            return;
        }
        reply = *held;
    }
    match reply {
        Reply::Status {
            status,
            headers,
            body,
            hold_open,
            delay,
        } => {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let mut head = format!("HTTP/1.1 {status} Scripted\r\nConnection: close\r\n");
            if !hold_open {
                let _ = write!(head, "Content-Length: {}\r\n", body.len());
            }
            if !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            {
                head.push_str("Content-Type: application/json\r\n");
            }
            for (name, value) in headers {
                let _ = write!(head, "{name}: {value}\r\n");
            }
            head.push_str("\r\n");
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(body.as_bytes()).await;
            if hold_open {
                let _ = stream.flush().await;
                hold(&mut stream, &mut signal).await;
            }
        }
        Reply::CutOff {
            status,
            headers,
            body,
        } => {
            let mut head = format!(
                "HTTP/1.1 {status} Scripted\r\nConnection: close\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n",
                body.len() + 1
            );
            for (name, value) in headers {
                let _ = write!(head, "{name}: {value}\r\n");
            }
            head.push_str("\r\n");
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(body.as_bytes()).await;
        }
        Reply::Disconnect | Reply::Gated { .. } => {}
        Reply::Raw(bytes) => {
            let _ = stream.write_all(&bytes).await;
        }
        Reply::Stream {
            chunks,
            hold_open,
            flushed,
        } => {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
            if stream.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            if !write_stream_chunks(&mut stream, &chunks, flushed.as_ref()).await {
                return;
            }
            if hold_open {
                hold(&mut stream, &mut signal).await;
            }
        }
    }
    let _ = stream.shutdown().await;
}

async fn write_stream_chunks<W: AsyncWrite + Unpin>(
    stream: &mut W,
    chunks: &[Vec<u8>],
    flushed: Option<&Gate>,
) -> bool {
    for chunk in chunks {
        if stream.write_all(chunk).await.is_err() || stream.flush().await.is_err() {
            return false;
        }
        if !chunk.is_empty()
            && let Some(flushed) = flushed
        {
            flushed.open();
        }
    }
    true
}

async fn hold<S: AsyncRead + Unpin>(stream: &mut S, signal: &mut watch::Receiver<bool>) {
    let mut sink = [0_u8; 64];
    tokio::select! {
        _ = signal.changed() => {}
        _ = stream.read(&mut sink) => {}
    }
}

async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> Option<RecordedRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let head_end = loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 || buffer.len() > MAX_REQUEST_BYTES {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_owned();
    let path = request_line.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Some(RecordedRequest {
        method,
        path,
        headers,
        body,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream as StdTcpStream;
    use std::time::Instant;

    use super::*;

    fn exchange(server: &FakeServer, request: &str) -> String {
        let address = server.base_url();
        let authority = address
            .trim_start_matches("http://")
            .trim_end_matches("/v1")
            .to_owned();
        let mut stream = StdTcpStream::connect(authority).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[tokio::test]
    async fn streamed_payload_notifications_belong_to_their_reply_and_follow_flush() {
        let first = Gate::default();
        let second = Gate::default();
        let (mut stream, mut reader) = tokio::io::duplex(16);
        assert!(write_stream_chunks(&mut stream, &[b"one".to_vec()], Some(&first)).await);
        assert!(first.is_open());
        assert!(!second.is_open());
        let mut bytes = [0; 3];
        reader.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"one");
        assert!(write_stream_chunks(&mut stream, &[b"two".to_vec()], Some(&second)).await);
        assert!(second.is_open());
        reader.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"two");
    }

    struct FailedFlush;

    impl AsyncWrite for FailedFlush {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn notifications_require_a_nonempty_payload_and_successful_flush() {
        let flushed = Gate::default();
        assert!(write_stream_chunks(&mut tokio::io::sink(), &[Vec::new()], Some(&flushed)).await);
        assert!(!flushed.is_open());
        assert!(
            !write_stream_chunks(&mut FailedFlush, &[b"payload".to_vec()], Some(&flushed)).await
        );
        assert!(!flushed.is_open());
    }

    #[tokio::test]
    async fn partial_payload_writes_never_publish_a_flush_notification() {
        let flushed = Gate::default();
        let (mut stream, mut reader) = tokio::io::duplex(1);
        let chunks = [b"payload".to_vec()];
        let (written, ()) = tokio::join!(
            write_stream_chunks(&mut stream, &chunks, Some(&flushed)),
            async move {
                let mut byte = [0];
                reader.read_exact(&mut byte).await.unwrap();
                assert_eq!(byte, [b'p']);
                drop(reader);
            }
        );
        assert!(!written);
        assert!(!flushed.is_open());
    }

    #[test]
    fn records_requests_and_serves_scripted_streams() {
        let server = FakeServer::start([Reply::sse(&["one", "[DONE]"])]);
        let response = exchange(
            &server,
            "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nX-Key: secret\r\nContent-Length: 2\r\n\r\n{}",
        );
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.ends_with("data: one\n\ndata: [DONE]\n\n"));
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/v1/chat/completions");
        assert_eq!(requests[0].header("x-key"), Some("secret"));
        assert_eq!(requests[0].json(), serde_json::json!({}));
    }

    #[test]
    fn serves_statuses_and_reports_missing_scripts() {
        let server = FakeServer::start([Reply::status_with_headers(
            429,
            &[("Retry-After", "3")],
            "{}",
        )]);
        let limited = exchange(&server, "GET / HTTP/1.1\r\n\r\n");
        assert!(limited.starts_with("HTTP/1.1 429"));
        assert!(limited.contains("Retry-After: 3"));
        assert!(limited.contains("Content-Type: application/json"));
        let missing = exchange(&server, "GET / HTTP/1.1\r\n\r\n");
        assert!(missing.starts_with("HTTP/1.1 500"));
    }

    #[test]
    fn delays_scripted_statuses() {
        let delay = Duration::from_millis(100);
        let server = FakeServer::start([Reply::delayed_status(200, "{}", delay)]);
        let started = Instant::now();
        let response = exchange(&server, "GET / HTTP/1.1\r\n\r\n");
        assert!(started.elapsed() >= delay);
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.ends_with("{}"));
    }

    #[test]
    fn serves_custom_content_types_and_disconnects() {
        let server = FakeServer::start([
            Reply::status_with_headers(200, &[("Content-Type", "text/html")], "<html></html>"),
            Reply::Disconnect,
        ]);
        let page = exchange(&server, "GET / HTTP/1.1\r\n\r\n");
        assert!(page.contains("Content-Type: text/html"));
        assert!(!page.contains("application/json"));
        assert_eq!(exchange(&server, "GET / HTTP/1.1\r\n\r\n"), "");
        assert_eq!(server.requests().len(), 2);
    }
}
