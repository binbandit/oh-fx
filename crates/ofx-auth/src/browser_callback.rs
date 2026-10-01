use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_OPEN_CONNECTIONS: usize = 16;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(30);
const TERMINATOR: &[u8] = b"\r\n\r\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Response {
    Ok,
    Failed,
    Unrelated,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParseResult<C, E> {
    Accepted(C),
    Unrelated,
    Failed(E),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AwaitError<E> {
    Cancelled,
    ListenerFailed,
    Rejected(E),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindError {
    PortUnavailable,
    Failed,
}

pub(crate) type Classifier<C, E> = Arc<dyn Fn(&str) -> ParseResult<C, E> + Send + Sync>;

#[derive(Debug)]
pub(crate) struct Accepted<C> {
    stream: TcpStream,
    pub(crate) callback: C,
}

impl<C> Accepted<C> {
    pub(crate) async fn respond(mut self, outcome: Response) -> io::Result<()> {
        write_response(&mut self.stream, outcome).await
    }
}

#[derive(Debug)]
pub(crate) struct CallbackListener {
    listener: TcpListener,
    port: u16,
}

impl CallbackListener {
    pub(crate) async fn bind(ports: &[u16]) -> Result<Self, BindError> {
        for &port in ports {
            match TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await {
                Ok(listener) => {
                    let port = listener.local_addr().map_err(|_| BindError::Failed)?.port();
                    return Ok(Self { listener, port });
                }
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => {}
                Err(_) => return Err(BindError::Failed),
            }
        }
        Err(BindError::PortUnavailable)
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    pub(crate) async fn accept<C, E>(
        &self,
        classify: &Classifier<C, E>,
        cancel: &CancellationToken,
    ) -> Result<Accepted<C>, AwaitError<E>>
    where
        C: Send + 'static,
        E: Send + 'static,
    {
        let mut connections: JoinSet<Result<Option<Accepted<C>>, E>> = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(AwaitError::Cancelled),
                Some(finished) = connections.join_next(), if !connections.is_empty() => {
                    match finished {
                        Ok(Ok(Some(accepted))) => return Ok(accepted),
                        Ok(Err(error)) => return Err(AwaitError::Rejected(error)),
                        Ok(Ok(None)) | Err(_) => {}
                    }
                }
                incoming = self.listener.accept() => match incoming {
                    Ok((stream, _)) if connections.len() < MAX_OPEN_CONNECTIONS => {
                        connections.spawn(serve(stream, Arc::clone(classify)));
                    }
                    Ok(_) => {}
                    Err(error) if transient_accept_error(&error) => {}
                    Err(_) => return Err(AwaitError::ListenerFailed),
                },
            }
        }
    }
}

fn transient_accept_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
    )
}

enum Request {
    Callback(String),
    NotCallback,
    Silent,
}

async fn serve<C, E>(
    mut stream: TcpStream,
    classify: Classifier<C, E>,
) -> Result<Option<Accepted<C>>, E> {
    let target = match tokio::time::timeout(SOCKET_TIMEOUT, read_request(&mut stream)).await {
        Ok(Request::Callback(target)) => target,
        Ok(Request::NotCallback) => {
            let _ = write_response(&mut stream, Response::Unrelated).await;
            return Ok(None);
        }
        Ok(Request::Silent) | Err(_) => return Ok(None),
    };
    match classify(&target) {
        ParseResult::Accepted(callback) => Ok(Some(Accepted { stream, callback })),
        ParseResult::Unrelated => {
            let _ = write_response(&mut stream, Response::Unrelated).await;
            Ok(None)
        }
        ParseResult::Failed(error) => {
            let _ = write_response(&mut stream, Response::Failed).await;
            Err(error)
        }
    }
}

async fn read_request(stream: &mut TcpStream) -> Request {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let Ok(read) = stream.read(&mut chunk).await else {
            return Request::Silent;
        };
        if read == 0 {
            return if bytes.is_empty() {
                Request::Silent
            } else {
                Request::NotCallback
            };
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = find(&bytes, TERMINATOR) {
            bytes.truncate(end);
            return classify_request(&bytes);
        }
        if bytes.len() > MAX_REQUEST_BYTES {
            return Request::NotCallback;
        }
    }
}

fn classify_request(head: &[u8]) -> Request {
    let line_end = find(head, b"\r\n").unwrap_or(head.len());
    let Ok(line) = std::str::from_utf8(&head[..line_end]) else {
        return Request::NotCallback;
    };
    let mut parts = line.splitn(3, ' ');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("GET"), Some(target), Some(_)) => Request::Callback(target.to_owned()),
        _ => Request::NotCallback,
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn callback_page(title: &str, detail: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>oh-fx</title><style>:root{{color-scheme:light dark}}body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;background:#fff;color:#111;font:15px/1.6 ui-sans-serif,-apple-system,BlinkMacSystemFont,\"Segoe UI\",sans-serif}}@media(prefers-color-scheme:dark){{body{{background:#0b0b0c;color:#f4f4f5}}}}main{{text-align:center;padding:2rem;max-width:26rem}}h1{{margin:0 0 .5rem;font-size:1.125rem;font-weight:600;letter-spacing:-.01em}}p{{margin:0;font-size:.875rem;opacity:.62}}</style></head><body><main><h1>{title}</h1><p>{detail}</p></main></body></html>"
    )
}

async fn write_response(stream: &mut TcpStream, outcome: Response) -> io::Result<()> {
    let (status, body) = match outcome {
        Response::Ok => (
            "200 OK",
            callback_page(
                "Authorization complete",
                "Returning you to oh-fx. You can close this tab.",
            ),
        ),
        Response::Failed => (
            "400 Bad Request",
            callback_page("Authorization failed", "Return to oh-fx for details."),
        ),
        Response::Unrelated => (
            "404 Not Found",
            "<!doctype html><title>Not found</title>Not found.".to_owned(),
        ),
    };
    let reply = format!(
        "HTTP/1.1 {status}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let written = async {
        stream.write_all(reply.as_bytes()).await?;
        stream.flush().await
    };
    tokio::time::timeout(SOCKET_TIMEOUT, written)
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream as StdTcpStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Instant;

    use super::*;

    fn granted() -> Classifier<&'static str, &'static str> {
        Arc::new(|target: &str| {
            if target == "/callback?code=granted" {
                ParseResult::Accepted("granted")
            } else if target == "/callback?error=denied" {
                ParseResult::Failed("denied")
            } else {
                ParseResult::Unrelated
            }
        })
    }

    async fn listener() -> CallbackListener {
        CallbackListener::bind(&[0]).await.unwrap()
    }

    fn exchange(port: u16, request: &str) -> String {
        let mut stream = StdTcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    #[tokio::test]
    async fn browser_callback_outruns_an_idle_preconnect_held_open() {
        let listener = listener().await;
        let port = listener.port();
        let release = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&release);
        let probe = thread::spawn(move || {
            let _idle = StdTcpStream::connect(("127.0.0.1", port)).unwrap();
            let mut stream = StdTcpStream::connect(("127.0.0.1", port)).unwrap();
            stream
                .write_all(b"GET /callback?code=granted HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .unwrap();
            while !held.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
        });
        let started = Instant::now();
        let accepted = listener
            .accept(&granted(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(accepted.callback, "granted");
        assert!(started.elapsed() < Duration::from_secs(1));
        release.store(true, Ordering::SeqCst);
        probe.join().unwrap();
    }

    #[tokio::test]
    async fn browser_callback_cancels_while_an_idle_preconnect_is_open() {
        let listener = listener().await;
        let port = listener.port();
        let idle = StdTcpStream::connect(("127.0.0.1", port)).unwrap();
        let cancel = CancellationToken::new();
        let flip = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            flip.cancel();
        });
        let started = Instant::now();
        assert_eq!(
            listener.accept(&granted(), &cancel).await.unwrap_err(),
            AwaitError::Cancelled
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(idle);
    }

    #[tokio::test]
    async fn browser_callback_survives_unrelated_requests_before_the_redirect() {
        let listener = listener().await;
        let port = listener.port();
        let probe = thread::spawn(move || {
            drop(StdTcpStream::connect(("127.0.0.1", port)).unwrap());
            let favicon = exchange(port, "GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
            let unrelated = exchange(
                port,
                "GET /unrelated?code=nope HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            );
            let posted = exchange(port, "POST /callback?code=granted HTTP/1.1\r\n\r\n");
            let callback = exchange(
                port,
                "GET /callback?code=granted HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            );
            (favicon, unrelated, posted, callback)
        });
        let accepted = listener
            .accept(&granted(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(accepted.callback, "granted");
        accepted.respond(Response::Ok).await.unwrap();
        let (favicon, unrelated, posted, callback) =
            tokio::task::spawn_blocking(move || probe.join().unwrap())
                .await
                .unwrap();
        for refused in [favicon, unrelated, posted] {
            assert!(
                refused.starts_with("HTTP/1.1 404 Not Found\r\n"),
                "{refused}"
            );
        }
        assert!(callback.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(callback.contains("Cache-Control: no-store\r\n"));
        assert!(callback.contains("Content-Type: text/html; charset=utf-8\r\n"));
        assert!(callback.contains("<h1>Authorization complete</h1>"));
        assert!(callback.contains("prefers-color-scheme:dark"));
    }

    async fn expect_reset_preconnect_survives(hold: Duration) {
        let listener = listener().await;
        let port = listener.port();
        let reset = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        reset.set_zero_linger().unwrap();
        drop(reset);
        let probe = tokio::task::spawn_blocking(move || {
            thread::sleep(hold);
            exchange(
                port,
                "GET /callback?code=granted HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            )
        });
        let accepted = listener
            .accept(&granted(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(accepted.callback, "granted");
        accepted.respond(Response::Ok).await.unwrap();
        assert!(probe.await.unwrap().starts_with("HTTP/1.1 200 OK\r\n"));
    }

    #[tokio::test]
    async fn browser_callback_survives_a_reset_preconnect_before_the_redirect() {
        expect_reset_preconnect_survives(Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn browser_callback_survives_a_reset_queued_before_accept() {
        expect_reset_preconnect_survives(Duration::ZERO).await;
    }

    #[tokio::test]
    async fn browser_callback_reports_a_rejected_callback_with_a_failure_page() {
        let listener = listener().await;
        let port = listener.port();
        let probe =
            thread::spawn(move || exchange(port, "GET /callback?error=denied HTTP/1.1\r\n\r\n"));
        assert_eq!(
            listener
                .accept(&granted(), &CancellationToken::new())
                .await
                .unwrap_err(),
            AwaitError::Rejected("denied")
        );
        let page = tokio::task::spawn_blocking(move || probe.join().unwrap())
            .await
            .unwrap();
        assert!(page.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(page.contains("<h1>Authorization failed</h1>"));
    }

    #[tokio::test]
    async fn browser_callback_binds_loopback_only_and_reports_busy_ports() {
        let listener = listener().await;
        let address = listener.listener.local_addr().unwrap();
        assert_eq!(address.ip(), Ipv4Addr::LOCALHOST);
        assert_eq!(
            CallbackListener::bind(&[listener.port()])
                .await
                .unwrap_err(),
            BindError::PortUnavailable
        );
    }
}
