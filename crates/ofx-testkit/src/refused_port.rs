use std::net::{SocketAddr, TcpListener};

pub struct RefusedPort {
    address: SocketAddr,
}

impl RefusedPort {
    pub fn reserve() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind a loopback port");
        let address = listener.local_addr().expect("read the bound port");
        drop(listener);
        Self { address }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn a_reserved_port_refuses_connections_at_once() {
        let port = RefusedPort::reserve();
        let started = Instant::now();
        assert!(TcpStream::connect(port.address).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(port.base_url().starts_with("http://127.0.0.1:"));
    }
}
