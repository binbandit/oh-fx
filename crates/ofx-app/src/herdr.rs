use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use ofx_tui::ForegroundLifecycle;

#[derive(Clone, Copy)]
pub(crate) enum State {
    Idle,
    Working,
    Blocked,
}

pub(crate) trait Reporter: Send + Sync {
    fn report(&self, state: State, status: Option<&[u8]>);
}

pub(crate) struct Herdr {
    connection: Mutex<Connection>,
}

struct Connection {
    socket: PathBuf,
    pane: Vec<u8>,
    next_id: u64,
    closed: bool,
}

enum Request<'a> {
    State(State, Option<&'a [u8]>),
    Session(&'a [u8]),
    Pane(Option<&'a [u8]>),
    Agent(Option<&'a [u8]>),
    Clear,
}

impl Herdr {
    pub(crate) fn from_env() -> Option<Self> {
        let toggle = std::env::var_os("OH_FX_HERDR");
        let socket = std::env::var_os("HERDR_SOCKET_PATH");
        let pane = std::env::var_os("HERDR_PANE_ID");
        Self::new(toggle.as_deref(), socket.as_deref(), pane.as_deref())
    }

    fn new(toggle: Option<&OsStr>, socket: Option<&OsStr>, pane: Option<&OsStr>) -> Option<Self> {
        if !enabled_bytes(toggle, socket, pane) {
            return None;
        }
        Some(Self {
            connection: Mutex::new(Connection {
                socket: socket?.into(),
                pane: pane?.as_encoded_bytes().to_vec(),
                next_id: 1,
                closed: false,
            }),
        })
    }

    pub(crate) fn initialize(&self, session: Option<&str>) {
        if let Some(session) = session.filter(|id| !id.is_empty()) {
            self.send(&[Request::Session(session.as_bytes())]);
        }
        self.report(State::Idle, None);
        self.send(&[Request::Pane(Some(b"fx")), Request::Agent(Some(b"fx"))]);
    }

    fn send(&self, requests: &[Request<'_>]) {
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !connection.closed {
            for request in requests {
                let _ = connection.send(request);
            }
        }
    }

    fn release(&self) {
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !connection.closed {
            connection.closed = true;
            for request in [Request::Agent(None), Request::Clear, Request::Pane(None)] {
                let _ = connection.send(&request);
            }
        }
    }
}

impl ForegroundLifecycle for Herdr {
    fn shutdown(&self) {
        self.release();
    }
}

impl Reporter for Herdr {
    fn report(&self, state: State, status: Option<&[u8]>) {
        let status = status
            .filter(|value| !value.is_empty())
            .map(|value| &value[..value.len().min(32)]);
        self.send(&[Request::State(state, status)]);
    }
}

pub(crate) struct HerdrObserver(pub(crate) std::sync::Arc<Herdr>);

impl ForegroundLifecycle for HerdrObserver {
    fn shutdown(&self) {
        self.0.shutdown();
    }
}

impl Drop for Herdr {
    fn drop(&mut self) {
        self.release();
    }
}

impl Connection {
    fn send(&mut self, request: &Request<'_>) -> io::Result<()> {
        let mut socket = UnixStream::connect(&self.socket)?;
        let _ = socket.set_read_timeout(Some(Duration::from_millis(250)));
        let id = self.next_id;
        self.next_id += 1;
        let mut output = io::BufWriter::with_capacity(1024, &mut socket);
        write!(output, "{{\"id\":\"{id}\",\"method\":")?;
        let method = match request {
            Request::State(..) => b"pane.report_agent".as_slice(),
            Request::Session(_) => b"pane.report_agent_session",
            Request::Pane(_) => b"pane.rename",
            Request::Agent(_) => b"agent.rename",
            Request::Clear => b"pane.clear_agent_authority",
        };
        write_string(&mut output, method)?;
        output.write_all(b",\"params\":{")?;
        write_string(
            &mut output,
            if matches!(request, Request::Agent(_)) {
                b"target"
            } else {
                b"pane_id"
            },
        )?;
        output.write_all(b":")?;
        write_string(&mut output, &self.pane)?;
        match request {
            Request::State(state, status) => {
                output.write_all(b",\"source\":\"custom:fx\",\"agent\":\"fx\",\"state\":")?;
                write_string(
                    &mut output,
                    match state {
                        State::Idle => b"idle",
                        State::Working => b"working",
                        State::Blocked => b"blocked",
                    },
                )?;
                if let Some(status) = status {
                    output.write_all(b",\"custom_status\":")?;
                    write_string(&mut output, status)?;
                }
            }
            Request::Session(session) => {
                output.write_all(
                    b",\"source\":\"custom:fx\",\"agent\":\"fx\",\"agent_session_id\":",
                )?;
                write_string(&mut output, session)?;
            }
            Request::Pane(value) | Request::Agent(value) => {
                output.write_all(if matches!(request, Request::Pane(_)) {
                    b",\"label\":"
                } else {
                    b",\"name\":"
                })?;
                match value {
                    Some(value) => write_string(&mut output, value)?,
                    None => output.write_all(b"null")?,
                }
            }
            Request::Clear => output.write_all(b",\"source\":\"custom:fx\"")?,
        }
        output.write_all(b"}}\n")?;
        output.flush()?;
        drop(output);
        let mut reply = [0; 512];
        let mut received = 0;
        while received < reply.len() {
            match socket.read(&mut reply[received..]) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    if reply[received..received + count].contains(&b'\n') {
                        break;
                    }
                    received += count;
                }
            }
        }
        Ok(())
    }
}

fn write_string(output: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    output.write_all(b"\"")?;
    for byte in bytes {
        match byte {
            b'"' => output.write_all(b"\\\"")?,
            b'\\' => output.write_all(b"\\\\")?,
            b'\n' => output.write_all(b"\\n")?,
            b'\r' => output.write_all(b"\\r")?,
            b'\t' => output.write_all(b"\\t")?,
            8 => output.write_all(b"\\b")?,
            12 => output.write_all(b"\\f")?,
            byte if *byte < 32 => write!(output, "\\u{byte:04x}")?,
            byte => output.write_all(&[*byte])?,
        }
    }
    output.write_all(b"\"")
}

fn enabled_bytes(toggle: Option<&OsStr>, socket: Option<&OsStr>, pane: Option<&OsStr>) -> bool {
    !toggle.is_some_and(|value| {
        value.as_encoded_bytes() == b"0" || value.as_encoded_bytes().eq_ignore_ascii_case(b"false")
    }) && socket.is_some_and(|value| !value.is_empty())
        && pane.is_some_and(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_out_and_missing_identity_match_upstream() {
        let socket = Some(OsStr::new("/socket"));
        let pane = Some(OsStr::new("pane"));
        for toggle in [None, Some("yes"), Some(""), Some(" false ")] {
            assert!(enabled_bytes(toggle.map(OsStr::new), socket, pane));
        }
        for toggle in ["0", "false", "FALSE", "False"] {
            assert!(!enabled_bytes(Some(OsStr::new(toggle)), socket, pane));
        }
        for (socket, pane) in [
            (None, pane),
            (socket, None),
            (Some(OsStr::new("")), pane),
            (socket, Some(OsStr::new(""))),
        ] {
            assert!(!enabled_bytes(None, socket, pane));
        }
    }

    #[test]
    fn json_strings_escape_controls_and_preserve_truncated_raw_utf8() {
        let mut output = Vec::new();
        write_string(&mut output, b"pane\"\\\n\t\r\x08\x0c\x01").unwrap();
        assert_eq!(output, b"\"pane\\\"\\\\\\n\\t\\r\\b\\f\\u0001\"");
        let text = format!("{}é", "x".repeat(31));
        output.clear();
        write_string(&mut output, &text.as_bytes()[..32]).unwrap();
        let mut expected = vec![b'"'];
        expected.extend_from_slice(&text.as_bytes()[..32]);
        expected.push(b'"');
        assert_eq!(output, expected);
        assert!(std::str::from_utf8(&output).is_err());
    }
    fn capture(reply: bool, count: usize, run: impl FnOnce(&mut Herdr)) -> Vec<Vec<u8>> {
        capture_after(Duration::ZERO, reply, count, run)
    }

    fn capture_after(
        accept_delay: Duration,
        reply: bool,
        count: usize,
        run: impl FnOnce(&mut Herdr),
    ) -> Vec<Vec<u8>> {
        use std::io::{BufRead, BufReader};
        let root = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(root.path())
            .unwrap()
            .join("herdr.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = std::sync::Arc::clone(&finished);
        let worker = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut lines = Vec::new();
            while started.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(accept_delay);
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket.set_nonblocking(false).unwrap();
                        let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
                        let mut line = Vec::new();
                        BufReader::new(socket.try_clone().unwrap())
                            .read_until(b'\n', &mut line)
                            .unwrap();
                        lines.push(line);
                        if reply {
                            let _ = socket.write_all(b"{}\n");
                        } else {
                            let _ = socket.read(&mut [0]);
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if done.load(std::sync::atomic::Ordering::Acquire) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            }
            lines
        });
        let mut client =
            Herdr::new(None, Some(path.as_os_str()), Some(OsStr::new("pane\"x"))).unwrap();
        run(&mut client);
        drop(client);
        finished.store(true, std::sync::atomic::Ordering::Release);
        let lines = worker.join().unwrap();
        assert_eq!(lines.len(), count);
        lines
    }

    #[test]
    fn socket_lifecycle_serializes_startup_reports_and_release() {
        let lines = capture(true, 10, |client| {
            client.initialize(Some("session-42"));
            client.report(State::Working, Some(b""));
            client.report(State::Blocked, Some(b"permission"));
            client.report(State::Idle, None);
        });
        assert_eq!(lines[0], b"{\"id\":\"1\",\"method\":\"pane.report_agent_session\",\"params\":{\"pane_id\":\"pane\\\"x\",\"source\":\"custom:fx\",\"agent\":\"fx\",\"agent_session_id\":\"session-42\"}}\n");
        let parsed: Vec<serde_json::Value> = lines
            .iter()
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(
            parsed
                .iter()
                .map(|value| value["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"]
        );
        assert_eq!(parsed[1]["params"]["state"], "idle");
        assert_eq!(parsed[2]["method"], "pane.rename");
        assert_eq!(parsed[2]["params"]["label"], "fx");
        assert_eq!(parsed[3]["method"], "agent.rename");
        assert_eq!(parsed[3]["params"]["name"], "fx");
        assert_eq!(parsed[4]["params"]["state"], "working");
        assert!(parsed[4]["params"].get("custom_status").is_none());
        assert_eq!(parsed[5]["params"]["custom_status"], "permission");
        assert_eq!(parsed[7]["method"], "agent.rename");
        assert!(parsed[7]["params"]["name"].is_null());
        assert_eq!(parsed[8]["method"], "pane.clear_agent_authority");
        assert_eq!(parsed[8]["params"]["source"], "custom:fx");
        assert!(parsed[9]["params"]["label"].is_null());
    }

    #[test]
    fn shutdown_releases_once_and_suppresses_later_reports_and_initialization() {
        let lines = capture(true, 4, |client| {
            client.report(State::Working, None);
            client.shutdown();
            client.shutdown();
            client.report(State::Idle, None);
            client.report(State::Working, Some(b"late"));
            client.report(State::Blocked, Some(b"permission"));
            client.initialize(Some("late-session"));
        });
        let parsed: Vec<serde_json::Value> = lines
            .iter()
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(parsed[0]["params"]["state"], "working");
        assert_eq!(parsed[1]["method"], "agent.rename");
        assert!(parsed[1]["params"]["name"].is_null());
        assert_eq!(parsed[2]["method"], "pane.clear_agent_authority");
        assert_eq!(parsed[3]["method"], "pane.rename");
        assert!(parsed[3]["params"]["label"].is_null());
        assert_eq!(parsed[3]["id"], "4");
    }

    #[test]
    fn socket_status_clamps_to_raw_bytes_inside_utf8() {
        let text = format!("{}é", "x".repeat(31));
        let lines = capture(true, 4, |client| {
            client.report(State::Blocked, Some(text.as_bytes()));
        });
        let mut expected = b"{\"id\":\"1\",\"method\":\"pane.report_agent\",\"params\":{\"pane_id\":\"pane\\\"x\",\"source\":\"custom:fx\",\"agent\":\"fx\",\"state\":\"blocked\",\"custom_status\":\"".to_vec();
        expected.extend_from_slice(&text.as_bytes()[..32]);
        expected.extend_from_slice(b"\"}}\n");
        assert_eq!(lines[0], expected);
    }

    #[test]
    fn never_replying_peer_uses_only_a_receive_timeout() {
        let lines = capture(false, 4, |client| {
            let started = std::time::Instant::now();
            client.report(State::Working, None);
            assert!(started.elapsed() >= Duration::from_millis(200));
            assert!(started.elapsed() < Duration::from_secs(2));
        });
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&lines[0]).unwrap()["params"]["state"],
            "working"
        );
    }

    #[test]
    fn a_peer_slower_than_the_receive_timeout_still_gets_every_request() {
        let lines = capture_after(Duration::from_millis(300), true, 4, |client| {
            client.report(State::Working, None);
        });
        let states: Vec<serde_json::Value> = lines
            .iter()
            .map(|line| {
                serde_json::from_slice::<serde_json::Value>(line).unwrap()["method"].clone()
            })
            .collect();
        assert_eq!(
            states,
            [
                "pane.report_agent",
                "agent.rename",
                "pane.clear_agent_authority",
                "pane.rename"
            ]
        );
    }

    #[test]
    fn failed_connection_does_not_consume_an_id() {
        let root = tempfile::tempdir().unwrap();
        let mut connection = Connection {
            socket: root.path().join("missing"),
            pane: b"pane".to_vec(),
            next_id: 1,
            closed: false,
        };
        assert!(connection.send(&Request::State(State::Idle, None)).is_err());
        assert_eq!(connection.next_id, 1);
    }
}
