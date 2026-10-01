use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::sleep;

#[derive(Debug, Clone)]
pub(crate) struct RecordedRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: String,
}

impl RecordedRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn method_name(&self) -> Option<String> {
        let value: serde_json::Value = serde_json::from_str(&self.body).ok()?;
        value.get("method")?.as_str().map(str::to_owned)
    }

    pub(crate) fn request_id(&self) -> Option<u64> {
        let value: serde_json::Value = serde_json::from_str(&self.body).ok()?;
        value.get("id")?.as_u64()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    parts: Vec<Vec<u8>>,
    streamed: bool,
    hold_open: bool,
}

impl Reply {
    pub(crate) fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            parts: Vec::new(),
            streamed: false,
            hold_open: false,
        }
    }

    pub(crate) fn json(body: &str) -> Self {
        Self {
            parts: vec![body.as_bytes().to_vec()],
            ..Self::status(200)
        }
        .header("Content-Type", "application/json")
    }

    pub(crate) fn sse(events: &[&str]) -> Self {
        Self {
            parts: events
                .iter()
                .map(|event| event.as_bytes().to_vec())
                .collect(),
            streamed: true,
            ..Self::status(200)
        }
        .header("Content-Type", "text/event-stream")
    }

    pub(crate) fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub(crate) fn held_open(mut self) -> Self {
        self.hold_open = true;
        self
    }
}

type Handler = Arc<dyn Fn(&RecordedRequest) -> Reply + Send + Sync>;

pub(crate) struct FakeServer {
    pub(crate) url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    task: JoinHandle<()>,
}

impl FakeServer {
    pub(crate) async fn start(
        handler: impl Fn(&RecordedRequest) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Handler = Arc::new(handler);
        let recorded = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let handler = Arc::clone(&handler);
                let recorded = Arc::clone(&recorded);
                tokio::spawn(serve(stream, handler, recorded));
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }

    pub(crate) fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.requests).clone()
    }

    pub(crate) async fn wait_for(&self, predicate: impl Fn(&RecordedRequest) -> bool) -> bool {
        for _ in 0..100 {
            if self.requests().iter().any(&predicate) {
                return true;
            }
            sleep(Duration::from_millis(20)).await;
        }
        false
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(
    mut stream: TcpStream,
    handler: Handler,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    lock(&recorded).push(request.clone());
    let reply = handler(&request);
    let mut head = format!("HTTP/1.1 {} Fake\r\nConnection: close\r\n", reply.status);
    for (name, value) in &reply.headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    if !reply.streamed {
        let length: usize = reply.parts.iter().map(Vec::len).sum();
        let _ = write!(head, "Content-Length: {length}\r\n");
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    for part in &reply.parts {
        if stream.write_all(part).await.is_err() {
            return;
        }
        let _ = stream.flush().await;
        if reply.streamed {
            sleep(Duration::from_millis(10)).await;
        }
    }
    if reply.hold_open {
        sleep(Duration::from_secs(30)).await;
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        let count = stream.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..count]);
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
    let length: usize = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let count = stream.read(&mut chunk).await.ok()?;
        if count == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..count]);
    }
    Some(RecordedRequest {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
