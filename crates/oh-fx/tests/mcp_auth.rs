use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ofx_testkit::{FakeServer, Reply, chat_text_events};
use serde_json::{Value, json};

const ACCESS: &str = "granted-access-token";
const REFRESH: &str = "granted-refresh-token";

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    body: String,
    authorization: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct Shape {
    protected_resource: &'static str,
    issuer_suffix: &'static str,
}

const PLAIN: Shape = Shape {
    protected_resource: "/mcp",
    issuer_suffix: "",
};

struct Authority {
    origin: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    shape: Arc<Mutex<Shape>>,
}

impl Authority {
    fn start() -> Self {
        Self::shaped(PLAIN)
    }

    fn shaped(shape: Shape) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the authority");
        let origin = format!(
            "http://{}",
            listener.local_addr().expect("authority address")
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let shape = Arc::new(Mutex::new(shape));
        let current = Arc::clone(&shape);
        let served = origin.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shape = *current.lock().expect("authority shape");
                answer(stream, &served, shape, &recorded);
            }
        });
        Self {
            origin,
            seen,
            shape,
        }
    }

    fn reshape(&self, shape: Shape) {
        *self.shape.lock().expect("authority shape") = shape;
    }

    fn paths(&self) -> Vec<String> {
        self.seen
            .lock()
            .expect("recorded requests")
            .iter()
            .map(|seen| format!("{} {}", seen.method, seen.path))
            .collect()
    }

    fn mcp_authorizations(&self) -> Vec<Option<String>> {
        self.seen
            .lock()
            .expect("recorded requests")
            .iter()
            .filter(|seen| seen.method == "POST" && seen.path == "/mcp")
            .map(|seen| seen.authorization.clone())
            .collect()
    }
}

struct Answer {
    status: &'static str,
    headers: Vec<String>,
    body: String,
}

impl Answer {
    fn json(status: &'static str, body: &Value) -> Self {
        Self {
            status,
            headers: vec!["Content-Type: application/json".to_owned()],
            body: body.to_string(),
        }
    }

    fn empty(status: &'static str) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: String::new(),
        }
    }
}

fn answer(mut stream: TcpStream, origin: &str, shape: Shape, seen: &Mutex<Vec<Seen>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("authority stream"));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut length = 0;
    let mut authorization = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header == "\r\n" || header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
            }
        }
    }
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    let body = String::from_utf8_lossy(&body).into_owned();
    seen.lock().expect("recorded requests").push(Seen {
        method: method.clone(),
        path: path.clone(),
        body: body.clone(),
        authorization: authorization.clone(),
    });
    let reply = match (method.as_str(), path.as_str()) {
        ("GET", "/.well-known/oauth-protected-resource/mcp") => Answer::json(
            "200 OK",
            &json!({"resource": format!("{origin}{}", shape.protected_resource), "authorization_servers": [origin]}),
        ),
        ("GET", "/.well-known/oauth-authorization-server") => Answer::json(
            "200 OK",
            &json!({
                "issuer": format!("{origin}{}", shape.issuer_suffix),
                "authorization_endpoint": format!("{origin}/authorize"),
                "token_endpoint": format!("{origin}/token"),
                "registration_endpoint": format!("{origin}/register"),
                "code_challenge_methods_supported": ["S256"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "token_endpoint_auth_methods_supported": ["none"],
            }),
        ),
        ("POST", "/register") => {
            Answer::json("201 Created", &json!({"client_id": "registered-client"}))
        }
        ("POST", "/token") => Answer::json(
            "200 OK",
            &json!({"access_token": ACCESS, "refresh_token": REFRESH, "expires_in": 3600, "token_type": "Bearer"}),
        ),
        ("POST", "/mcp") => mcp_answer(&body, authorization.as_deref(), origin),
        ("DELETE", "/mcp") => Answer::empty("200 OK"),
        ("GET", "/mcp") => Answer::empty("405 Method Not Allowed"),
        _ => Answer::json("404 Not Found", &json!({})),
    };
    let mut head = format!("HTTP/1.1 {}\r\n", reply.status);
    for header in &reply.headers {
        head.push_str(header);
        head.push_str("\r\n");
    }
    let _ = write!(
        stream,
        "{head}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        reply.body.len(),
        reply.body
    );
}

fn mcp_answer(body: &str, authorization: Option<&str>, origin: &str) -> Answer {
    if authorization != Some(format!("Bearer {ACCESS}").as_str()) {
        let mut rejected = Answer::empty("401 Unauthorized");
        rejected.headers.push(format!(
            "WWW-Authenticate: Bearer resource_metadata=\"{origin}/.well-known/oauth-protected-resource/mcp\""
        ));
        return rejected;
    }
    let message: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let Some(id) = message.get("id").cloned() else {
        return Answer::empty("202 Accepted");
    };
    let result = match message["method"].as_str() {
        Some("initialize") => json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture", "version": "1.0"},
        }),
        Some("tools/list") => json!({"tools": []}),
        _ => json!({}),
    };
    Answer::json(
        "200 OK",
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
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

    fn servers(&self, servers: &Value) {
        fs::write(
            self.root.join("config/oh-fx/mcp.json"),
            json!({ "mcp": servers }).to_string(),
        )
        .expect("profile servers");
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    fn settings(&self, settings: &Value) {
        fs::write(
            self.root.join("config/oh-fx/settings.json"),
            settings.to_string(),
        )
        .expect("profile settings");
    }

    fn ask(&self, model: &FakeServer) -> std::process::Output {
        self.settings(&json!({
            "permission_mode": "ask",
            "provider": "local",
            "model": "local-model",
            "providers": {"local": {
                "protocol": "openai-chat-completions",
                "base_url": model.base_url(),
                "auth": {"type": "none"},
                "models": ["local-model"],
            }},
        }));
        self.command(&["ask", "hi"])
            .output()
            .expect("run oh-fx ask")
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

fn authorize(home: &Home, name: &str) -> (std::process::Output, String, String) {
    let mut child = home
        .command(&["mcp", "auth", name])
        .spawn()
        .expect("test step");
    let mut stdout = BufReader::new(child.stdout.take().expect("test step"));
    let (url, shown) = authorization_url(&mut stdout);
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
    (output, shown, tail)
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("test step").permissions().mode() & 0o777
}

#[test]
fn mcp_auth_authorizes_in_the_browser_and_stores_the_grant_privately() {
    let authority = Authority::start();
    let home = Home::new(&authority.origin);
    let (output, shown, tail) = authorize(&home, "fixture");
    let url = shown.lines().nth(1).expect("test step");
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

#[test]
fn a_granted_server_connects_with_its_bearer_whatever_identity_discovery_accepted() {
    let enclosing = Shape {
        protected_resource: "/",
        issuer_suffix: "",
    };
    let slashed = Shape {
        protected_resource: "/mcp",
        issuer_suffix: "/",
    };
    for (shape, oauth) in [
        (PLAIN, None),
        (enclosing, Some("resource")),
        (slashed, Some("issuer")),
    ] {
        let authority = Authority::shaped(shape);
        let home = Home::new(&authority.origin);
        let mut server = json!({
            "type": "http",
            "url": format!("{}/mcp", authority.origin),
            "required": true,
        });
        match oauth {
            Some("resource") => {
                server["oauth"] = json!({"resource": format!("{}/mcp", authority.origin)});
            }
            Some(_) => {
                server["oauth"] = json!({"issuer": authority.origin});
            }
            None => {}
        }
        home.servers(&json!({"fixture": server}));
        let (granted, _, tail) = authorize(&home, "fixture");
        assert!(
            granted.status.success(),
            "{shape:?}: {}",
            String::from_utf8_lossy(&granted.stderr)
        );
        assert_eq!(tail, "Authenticated MCP server 'fixture'.\n", "{shape:?}");
        let model = FakeServer::start([Reply::sse(&chat_text_events(&["done"]))]);
        let asked = home.ask(&model);
        let stderr = String::from_utf8_lossy(&asked.stderr);
        assert!(asked.status.success(), "{shape:?}: {stderr}");
        let sent = authority.mcp_authorizations();
        assert!(!sent.is_empty(), "{shape:?}");
        assert!(
            sent.iter()
                .all(|bearer| bearer.as_deref() == Some(format!("Bearer {ACCESS}").as_str())),
            "{shape:?}: {sent:?}"
        );
        for secret in [ACCESS, REFRESH] {
            assert!(!stderr.contains(secret), "{stderr}");
            assert!(
                !String::from_utf8_lossy(&asked.stdout).contains(secret),
                "{shape:?}"
            );
        }
    }
}

#[test]
fn mcp_auth_refuses_a_project_server_whose_grant_would_never_be_used() {
    let authority = Authority::start();
    let home = Home::new(&authority.origin);
    fs::write(
        home.workspace().join(".mcp.json"),
        json!({"mcpServers": {"project": {"type": "http", "url": format!("{}/mcp", authority.origin)}}})
            .to_string(),
    )
    .expect("project servers");
    home.settings(&json!({
        "workspaces": {home.workspace().display().to_string(): {"enabledMcpjsonServers": ["project"]}},
    }));
    let output = home
        .command(&["mcp", "auth", "project"])
        .output()
        .expect("test step");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "oh-fx mcp auth failed: McpStoredCredentialsNotAllowed.\n"
    );
    assert!(output.stdout.is_empty());
    assert!(authority.paths().is_empty());
    assert!(!home.store().exists());
}

#[test]
fn authorizing_again_after_discovery_changes_keeps_one_grant_that_connects() {
    let slashed = Shape {
        protected_resource: "/mcp",
        issuer_suffix: "/",
    };
    let enclosing = Shape {
        protected_resource: "/",
        issuer_suffix: "",
    };
    for (first, second) in [(slashed, PLAIN), (PLAIN, enclosing)] {
        let authority = Authority::shaped(first);
        let home = Home::new(&authority.origin);
        home.servers(&json!({"fixture": {
            "type": "http",
            "url": format!("{}/mcp", authority.origin),
            "required": true,
        }}));
        for shape in [first, second] {
            authority.reshape(shape);
            let (granted, _, tail) = authorize(&home, "fixture");
            assert!(
                granted.status.success(),
                "{shape:?}: {}",
                String::from_utf8_lossy(&granted.stderr)
            );
            assert_eq!(tail, "Authenticated MCP server 'fixture'.\n", "{shape:?}");
        }
        let stored: Value =
            serde_json::from_str(&fs::read_to_string(home.store()).expect("test step"))
                .expect("test step");
        assert_eq!(
            stored["credentials"].as_array().map(Vec::len),
            Some(1),
            "{first:?} then {second:?}"
        );
        let model = FakeServer::start([Reply::sse(&chat_text_events(&["done"]))]);
        let asked = home.ask(&model);
        assert!(
            asked.status.success(),
            "{first:?} then {second:?}: {}",
            String::from_utf8_lossy(&asked.stderr)
        );
        let sent = authority.mcp_authorizations();
        assert!(!sent.is_empty());
        assert!(
            sent.iter()
                .all(|bearer| bearer.as_deref() == Some(format!("Bearer {ACCESS}").as_str())),
            "{sent:?}"
        );
    }
}
