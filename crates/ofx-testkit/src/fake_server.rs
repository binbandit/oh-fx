use std::collections::VecDeque;
use std::fmt::Write;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Stream {
        chunks: Vec<Vec<u8>>,
        hold_open: bool,
    },
    Status {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
        hold_open: bool,
    },
    CutOff {
        status: u16,
        body: String,
    },
    Disconnect,
}

impl Reply {
    pub fn sse<S: AsRef<str>>(events: &[S]) -> Self {
        Self::Stream {
            chunks: sse_chunks(events),
            hold_open: false,
        }
    }

    pub fn held_sse<S: AsRef<str>>(events: &[S]) -> Self {
        Self::Stream {
            chunks: sse_chunks(events),
            hold_open: true,
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
        }
    }

    pub fn cut_off(status: u16, body: impl Into<String>) -> Self {
        Self::CutOff {
            status,
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
}

#[derive(Debug)]
pub struct FakeServer {
    address: SocketAddr,
    state: Arc<Mutex<State>>,
    shutdown: watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeServer {
    pub fn start(replies: impl IntoIterator<Item = Reply>) -> Self {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        listener
            .set_nonblocking(true)
            .expect("make the listener non-blocking");
        let address = listener.local_addr().expect("read the bound address");
        let state = Arc::new(Mutex::new(State {
            replies: replies.into_iter().collect(),
            requests: Vec::new(),
        }));
        let (shutdown, signal) = watch::channel(false);
        let served_state = Arc::clone(&state);
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build the fake server runtime");
            runtime.block_on(serve(listener, served_state, signal));
        });
        Self {
            address,
            state,
            shutdown,
            thread: Some(thread),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.state).requests.clone()
    }
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
                    tokio::spawn(handle(stream, Arc::clone(&state), signal.clone()));
                }
            }
        }
    }
}

async fn handle(
    mut stream: TcpStream,
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
    let reply = reply.unwrap_or_else(|| Reply::status(500, "no scripted reply"));
    match reply {
        Reply::Status {
            status,
            headers,
            body,
            hold_open,
        } => {
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
        Reply::CutOff { status, body } => {
            let head = format!(
                "HTTP/1.1 {status} Scripted\r\nConnection: close\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
                body.len() + 1
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(body.as_bytes()).await;
        }
        Reply::Disconnect => {}
        Reply::Stream { chunks, hold_open } => {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
            if stream.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            for chunk in chunks {
                if stream.write_all(&chunk).await.is_err() || stream.flush().await.is_err() {
                    return;
                }
            }
            if hold_open {
                hold(&mut stream, &mut signal).await;
            }
        }
    }
    let _ = stream.shutdown().await;
}

async fn hold(stream: &mut TcpStream, signal: &mut watch::Receiver<bool>) {
    let mut sink = [0_u8; 64];
    tokio::select! {
        _ = signal.changed() => {}
        _ = stream.read(&mut sink) => {}
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
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
