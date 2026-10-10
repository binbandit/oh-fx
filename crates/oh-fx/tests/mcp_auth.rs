use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

const ACCESS: &str = "granted-access-token";
const REFRESH: &str = "granted-refresh-token";

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    body: String,
}

struct Authority {
    origin: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Authority {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the authority");
        let origin = format!(
            "http://{}",
            listener.local_addr().expect("authority address")
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let served = origin.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                answer(stream, &served, &recorded);
            }
        });
        Self { origin, seen }
    }

    fn paths(&self) -> Vec<String> {
        self.seen
            .lock()
            .expect("recorded requests")
            .iter()
            .map(|seen| format!("{} {}", seen.method, seen.path))
            .collect()
    }
}

fn answer(mut stream: TcpStream, origin: &str, seen: &Mutex<Vec<Seen>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("authority stream"));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header == "\r\n" || header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    seen.lock().expect("recorded requests").push(Seen {
        method: method.clone(),
        path: path.clone(),
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    let (status, reply) = match (method.as_str(), path.as_str()) {
        ("GET", "/.well-known/oauth-protected-resource/mcp") => (
            "200 OK",
            json!({"resource": format!("{origin}/mcp"), "authorization_servers": [origin]}),
        ),
        ("GET", "/.well-known/oauth-authorization-server") => (
            "200 OK",
            json!({
                "issuer": origin,
                "authorization_endpoint": format!("{origin}/authorize"),
                "token_endpoint": format!("{origin}/token"),
                "registration_endpoint": format!("{origin}/register"),
                "code_challenge_methods_supported": ["S256"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "token_endpoint_auth_methods_supported": ["none"],
            }),
        ),
        ("POST", "/register") => ("201 Created", json!({"client_id": "registered-client"})),
        ("POST", "/token") => (
            "200 OK",
            json!({"access_token": ACCESS, "refresh_token": REFRESH, "expires_in": 3600, "token_type": "Bearer"}),
        ),
        _ => ("404 Not Found", json!({})),
    };
    let text = reply.to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new(origin: &str) -> Self {
        let directory = tempfile::tempdir().expect("temporary home");
        let root = fs::canonicalize(directory.path()).expect("canonical home");
        let config = root.join("config/oh-fx");
        fs::create_dir_all(&config).expect("config directory");
        fs::create_dir_all(root.join("workspace")).expect("workspace");
        fs::write(
            config.join("mcp.json"),
            json!({"mcp": {
                "fixture": {"type": "http", "url": format!("{origin}/mcp")},
                "local": {"command": "/bin/true"},
            }})
            .to_string(),
        )
        .expect("profile servers");
        Self {
            _directory: directory,
            root,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .args(args)
            .current_dir(self.root.join("workspace"))
            .env_clear()
            .env("HOME", &self.root)
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("OH_FX_AUTO_UPGRADE", "0")
            .env("OH_FX_NO_OPEN_BROWSER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn store(&self) -> PathBuf {
        self.root
            .join("data/oh-fx/mcp-credentials/credentials.json")
    }
}

fn authorization_url(stdout: &mut impl BufRead) -> (String, String) {
    let mut shown = String::new();
    while !shown.ends_with("Waiting for browser authorization...\n") {
        let mut line = String::new();
        assert!(
            stdout.read_line(&mut line).expect("test step") > 0,
            "{shown}"
        );
        shown.push_str(&line);
    }
    let url = shown.lines().nth(1).expect("test step").to_owned();
    (url, shown)
}

fn query(url: &str, key: &str) -> String {
    let (_, query) = url.split_once('?').expect("test step");
    let value = query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
        .expect("test step");
    let mut decoded = Vec::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            decoded.push(u8::from_str_radix(&value[index + 1..index + 3], 16).expect("test step"));
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).expect("test step")
}

fn visit(url: &str) -> String {
    let rest = url.strip_prefix("http://").expect("test step");
    let (authority, path) = rest.split_once('/').expect("test step");
    let mut stream = TcpStream::connect(authority).expect("test step");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("test step");
    write!(
        stream,
        "GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )
    .expect("test step");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("test step").permissions().mode() & 0o777
}

#[test]
fn mcp_auth_authorizes_in_the_browser_and_stores_the_grant_privately() {
    let authority = Authority::start();
    let home = Home::new(&authority.origin);
    let mut child = home
        .command(&["mcp", "auth", "fixture"])
        .spawn()
        .expect("test step");
    let mut stdout = BufReader::new(child.stdout.take().expect("test step"));
    let (url, shown) = authorization_url(&mut stdout);
    assert!(
        shown.starts_with("Open this URL to authenticate the MCP server:\n"),
        "{shown}"
    );
    assert!(
        shown.contains("\n\nWaiting for browser authorization...\n"),
        "{shown}"
    );
    assert!(
        url.starts_with(&format!("{}/authorize?", authority.origin)),
        "{url}"
    );
    let callback = format!(
        "{}?code=browser-code&state={}",
        query(&url, "redirect_uri"),
        query(&url, "state")
    );
    let page = visit(&callback);
    assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
    let mut tail = String::new();
    stdout.read_to_string(&mut tail).expect("test step");
    let output = child.wait_with_output().expect("test step");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(stderr, "");
    assert_eq!(tail, "Authenticated MCP server 'fixture'.\n");
    assert_eq!(
        authority.paths(),
        [
            "GET /.well-known/oauth-protected-resource/mcp",
            "GET /.well-known/oauth-authorization-server",
            "POST /register",
            "POST /token",
        ]
    );
    let token_form = authority.seen.lock().expect("recorded requests")[3]
        .body
        .clone();
    assert!(token_form.contains("code=browser-code"), "{token_form}");
    assert_eq!(mode(&home.store()), 0o600);
    assert_eq!(mode(home.store().parent().expect("test step")), 0o700);
    let stored: Value = serde_json::from_str(&fs::read_to_string(home.store()).expect("test step"))
        .expect("test step");
    let entry = &stored["credentials"][0];
    assert_eq!(entry["server_identity"], "fixture");
    assert_eq!(entry["endpoint"], format!("{}/mcp", authority.origin));
    assert_eq!(entry["client_id"], "registered-client");
    assert_eq!(entry["access_token"], ACCESS);
    assert_eq!(entry["refresh_token"], REFRESH);
    assert_eq!(entry["token_endpoint_auth_method"], "none");
    for secret in [ACCESS, REFRESH, "browser-code"] {
        assert!(!shown.contains(secret), "{shown}");
        assert!(!tail.contains(secret), "{tail}");
        assert!(!stderr.contains(secret), "{stderr}");
    }
}

#[test]
fn mcp_auth_refuses_unknown_and_local_servers() {
    let authority = Authority::start();
    let home = Home::new(&authority.origin);
    for (name, failure) in [
        ("absent", "oh-fx mcp auth failed: McpServerNotFound.\n"),
        (
            "local",
            "oh-fx mcp auth failed: McpAuthenticationNotRemote.\n",
        ),
    ] {
        let output = home
            .command(&["mcp", "auth", name])
            .output()
            .expect("test step");
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(String::from_utf8_lossy(&output.stderr), failure);
        assert!(output.stdout.is_empty());
    }
    let usage = home.command(&["mcp", "auth"]).output().expect("test step");
    assert_eq!(usage.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&usage.stderr),
        "usage: oh-fx mcp auth NAME\n"
    );
    assert!(authority.paths().is_empty());
    assert!(!home.store().exists());
}
