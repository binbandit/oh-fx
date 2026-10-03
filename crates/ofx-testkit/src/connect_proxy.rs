use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

const MAX_HEAD_BYTES: usize = 16 * 1024;

#[derive(Debug)]
pub struct ConnectProxy {
    address: SocketAddr,
    targets: Arc<Mutex<Vec<String>>>,
    shutdown: watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
}

impl ConnectProxy {
    pub fn start(upstream: SocketAddr) -> Self {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        listener
            .set_nonblocking(true)
            .expect("make the listener non-blocking");
        let address = listener.local_addr().expect("read the bound address");
        let targets = Arc::new(Mutex::new(Vec::new()));
        let (shutdown, signal) = watch::channel(false);
        let recorded = Arc::clone(&targets);
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build the proxy runtime");
            runtime.block_on(serve(listener, upstream, recorded, signal));
        });
        Self {
            address,
            targets,
            shutdown,
            thread: Some(thread),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn targets(&self) -> Vec<String> {
        self.targets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for ConnectProxy {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn serve(
    listener: StdTcpListener,
    upstream: SocketAddr,
    targets: Arc<Mutex<Vec<String>>>,
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
                    tokio::spawn(tunnel(stream, upstream, Arc::clone(&targets)));
                }
            }
        }
    }
}

async fn tunnel(mut client: TcpStream, upstream: SocketAddr, targets: Arc<Mutex<Vec<String>>>) {
    let mut head = Vec::new();
    let mut chunk = [0_u8; 1024];
    let head_end = loop {
        if let Some(position) = head.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        match client.read(&mut chunk).await {
            Ok(read) if read > 0 && head.len() < MAX_HEAD_BYTES => {
                head.extend_from_slice(&chunk[..read]);
            }
            _ => return,
        }
    };
    let request = String::from_utf8_lossy(&head[..head_end]).into_owned();
    let mut words = request.split(' ');
    if words.next() != Some("CONNECT") {
        let _ = client
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    }
    let target = words.next().unwrap_or_default().to_owned();
    targets
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(target);
    let Ok(mut server) = TcpStream::connect(upstream).await else {
        let _ = client
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    };
    if client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
        || server.write_all(&head[head_end..]).await.is_err()
    {
        return;
    }
    let _ = copy_bidirectional(&mut client, &mut server).await;
}
