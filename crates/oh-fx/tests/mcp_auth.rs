use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ofx_testkit::{FakeServer, PtySession, Reply, chat_text_events};
use serde_json::{Value, json};

const ACCESS: &str = "granted-access-token";
const REFRESH: &str = "granted-refresh-token";
const SHELL_WAIT: Duration = Duration::from_secs(15);

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
    revocation: &'static str,
}

const PLAIN: Shape = Shape {
    protected_resource: "/mcp",
    issuer_suffix: "",
    revocation: "200 OK",
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

    fn revocations(&self) -> Vec<String> {
        self.seen
            .lock()
            .expect("recorded requests")
            .iter()
            .filter(|seen| seen.method == "POST" && seen.path == "/revoke")
            .map(|seen| seen.body.clone())
            .collect()
    }

    fn closings(&self) -> Vec<Option<String>> {
        self.seen
            .lock()
            .expect("recorded requests")
            .iter()
            .filter(|seen| seen.method == "DELETE" && seen.path == "/mcp")
            .map(|seen| seen.authorization.clone())
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
                "revocation_endpoint": format!("{origin}/revoke"),
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
        ("POST", "/revoke") => Answer::empty(shape.revocation),
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
    let mut answer = Answer::json(
        "200 OK",
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    );
    if message["method"] == "initialize" {
        answer
            .headers
            .push("Mcp-Session-Id: fixture-session".to_owned());
    }
    answer
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

    fn use_model(&self, model: &FakeServer) {
        self.use_model_in(model, &json!({}));
    }

    fn use_model_in(&self, model: &FakeServer, workspaces: &Value) {
        self.settings(&json!({
            "workspaces": workspaces,
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
    }

    fn ask(&self, model: &FakeServer) -> std::process::Output {
        self.use_model(model);
        self.command(&["ask", "hi"])
            .output()
            .expect("run oh-fx ask")
    }

    fn opened(&self) -> PathBuf {
        self.root.join("opened")
    }

    fn shell(&self, model: &FakeServer, workspaces: &Value) -> PtySession {
        self.use_model_in(model, workspaces);
        let bin = self.root.join("bin");
        fs::create_dir_all(&bin).expect("test step");
        for launcher in ["open", "xdg-open"] {
            let path = bin.join(launcher);
            fs::write(
                &path,
                "#!/bin/sh\nprintf '%s\\n' \"$1\" > \"$OPENED.part\" && mv \"$OPENED.part\" \"$OPENED\"\n",
            )
            .expect("test step");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("test step");
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .current_dir(self.root.join("workspace"))
            .env_clear()
            .env("HOME", &self.root)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .env("OPENED", self.opened())
            .process_group(0);
        let session = PtySession::spawn(command, 40, 200).expect("spawn oh-fx in a pty");
        session
            .wait_for(SHELL_WAIT, |screen| {
                screen.contains("Run /help for commands")
            })
            .expect("the shell starts");
        session
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
        revocation: "200 OK",
    };
    let slashed = Shape {
        protected_resource: "/mcp",
        issuer_suffix: "/",
        revocation: "200 OK",
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
        revocation: "200 OK",
    };
    let enclosing = Shape {
        protected_resource: "/",
        issuer_suffix: "",
        revocation: "200 OK",
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

fn shown(session: &PtySession, needle: &str) -> String {
    session
        .wait_for(SHELL_WAIT, |screen| screen.contains(needle))
        .unwrap_or_else(|screen| panic!("expected {needle:?} on screen:\n{screen}"))
}

const MENU_HINT: &str = "esc close";

fn server_row(name: &str, state: &str, meta: &str) -> String {
    format!("  {name:<22}{state:<22}{meta}")
}

fn menu_shows(session: &PtySession, row: &str) {
    session.send(b"/mcp\r");
    session
        .wait_for(SHELL_WAIT, |screen| {
            screen.contains(MENU_HINT) && screen.contains(row)
        })
        .unwrap_or_else(|screen| panic!("expected {row:?} in the MCP menu:\n{screen}"));
}

fn close_menu(session: &PtySession) {
    session.send(b"\x1b");
    session
        .wait_for(SHELL_WAIT, |screen| !screen.contains(MENU_HINT))
        .unwrap_or_else(|screen| panic!("the MCP menu stayed open:\n{screen}"));
}

fn settled_menu(session: &PtySession, row: &str) {
    menu_shows(session, row);
    close_menu(session);
}

fn wait_for_file(path: &Path) -> String {
    let started = Instant::now();
    while started.elapsed() < SHELL_WAIT {
        if let Ok(text) = fs::read_to_string(path) {
            return text;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("{} was never written", path.display());
}

#[test]
fn mcp_auth_in_the_shell_confirms_opens_the_browser_and_reconnects_the_server() {
    let authority = Authority::start();
    let home = Home::new(&authority.origin);
    let model = FakeServer::start([]);
    let session = home.shell(&model, &json!({}));
    settled_menu(
        &session,
        &server_row("fixture", "Needs authentication", "HTTP · Profile"),
    );
    session.send(b"/mcp auth fixture\r");
    shown(
        &session,
        "Run /mcp auth fixture --open to confirm opening your browser.",
    );
    assert!(!home.opened().exists());
    session.send(b"/mcp auth fixture --open\r");
    shown(
        &session,
        "Waiting for MCP authentication for 'fixture'. You can continue using oh-fx while the browser flow completes.",
    );
    let url = wait_for_file(&home.opened()).trim().to_owned();
    assert!(
        url.starts_with(&format!("{}/authorize?", authority.origin)),
        "{url}"
    );
    let page = visit(&format!(
        "{}?code=browser-code&state={}",
        query(&url, "redirect_uri"),
        query(&url, "state")
    ));
    assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
    shown(&session, "Authenticated MCP server 'fixture'.");
    menu_shows(&session, &server_row("fixture", "Ready", "HTTP · Profile"));
    let screen = session.screen();
    for secret in [ACCESS, REFRESH, "browser-code", "authorize?"] {
        assert!(!screen.contains(secret), "{screen}");
    }
    assert!(
        authority
            .mcp_authorizations()
            .iter()
            .any(|bearer| bearer.as_deref() == Some(format!("Bearer {ACCESS}").as_str()))
    );
    assert_eq!(mode(&home.store()), 0o600);
}

#[test]
fn a_project_server_authenticated_in_the_shell_uses_its_grant_for_the_session() {
    let authority = Authority::start();
    let home = Home::new(&authority.origin);
    home.servers(&json!({}));
    fs::write(
        home.workspace().join(".mcp.json"),
        json!({"mcpServers": {"project": {"type": "http", "url": format!("{}/mcp", authority.origin)}}})
            .to_string(),
    )
    .expect("project servers");
    let model = FakeServer::start([]);
    let session = home.shell(
        &model,
        &json!({home.workspace().display().to_string(): {"enabledMcpjsonServers": ["project"]}}),
    );
    settled_menu(
        &session,
        &server_row("project", "Needs authentication", "HTTP · Project"),
    );
    session.send(b"/mcp auth project --open\r");
    shown(&session, "Waiting for MCP authentication for 'project'.");
    let url = wait_for_file(&home.opened()).trim().to_owned();
    let page = visit(&format!(
        "{}?code=browser-code&state={}",
        query(&url, "redirect_uri"),
        query(&url, "state")
    ));
    assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
    shown(&session, "Authenticated MCP server 'project'.");
    settled_menu(&session, &server_row("project", "Ready", "HTTP · Project"));
}

#[test]
fn the_mcp_menu_signs_in_a_server_from_its_details() {
    let authority = Authority::start();
    let home = Home::new(&authority.origin);
    let model = FakeServer::start([]);
    let session = home.shell(&model, &json!({}));
    menu_shows(
        &session,
        &server_row("fixture", "Needs authentication", "HTTP · Profile"),
    );
    session.send(b"\r");
    shown(&session, "enter sign in  c help  esc back");
    shown(
        &session,
        "  Error               Authentication is required or the saved credentials lack access",
    );
    session.send(b"\r");
    let url = wait_for_file(&home.opened()).trim().to_owned();
    assert!(
        url.starts_with(&format!("{}/authorize?", authority.origin)),
        "{url}"
    );
    let page = visit(&format!(
        "{}?code=browser-code&state={}",
        query(&url, "redirect_uri"),
        query(&url, "state")
    ));
    assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
    shown(&session, "  Authenticated MCP server 'fixture'.");
    shown(&session, "  State               Ready");
    shown(&session, "c help  esc back");
    let screen = session.screen();
    assert!(!screen.contains("enter sign in"), "{screen}");
    assert!(
        !screen.contains("Waiting for MCP authentication"),
        "{screen}"
    );
    for secret in [ACCESS, REFRESH, "browser-code", "authorize?"] {
        assert!(!screen.contains(secret), "{screen}");
    }
    session.send(b"\x1b");
    shown(&session, MENU_HINT);
    close_menu(&session);
    assert!(
        authority
            .mcp_authorizations()
            .iter()
            .any(|bearer| bearer.as_deref() == Some(format!("Bearer {ACCESS}").as_str()))
    );
    assert_eq!(mode(&home.store()), 0o600);
}

fn logout(home: &Home) -> std::process::Output {
    home.command(&["mcp", "logout", "fixture"])
        .env("OH_FX_TRACE_LOG", home.root.join("trace.log"))
        .env("OH_FX_TRACE_SCOPES", "mcp")
        .output()
        .expect("test step")
}

fn assert_secret_free(home: &Home, output: &std::process::Output) {
    let trace = fs::read_to_string(home.root.join("trace.log")).unwrap_or_default();
    for text in [
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        trace,
    ] {
        for secret in [ACCESS, REFRESH, "browser-code"] {
            assert!(!text.contains(secret), "{text}");
        }
    }
}

#[test]
fn mcp_logout_revokes_and_deletes_the_grant() {
    let authority = Authority::start();
    let home = Home::new(&authority.origin);
    let (granted, _, _) = authorize(&home, "fixture");
    assert!(granted.status.success());
    let output = logout(&home);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Logged out of MCP server 'fixture'.\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert!(!home.store().exists());
    let revocations = authority.revocations();
    assert_eq!(revocations.len(), 2);
    assert_eq!(
        revocations[0],
        format!("token={REFRESH}&token_type_hint=refresh_token&client_id=registered-client")
    );
    assert_eq!(
        revocations[1],
        format!("token={ACCESS}&token_type_hint=access_token&client_id=registered-client")
    );
    assert_secret_free(&home, &output);
    let again = logout(&home);
    assert_eq!(again.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&again.stdout),
        "No stored MCP credentials found for 'fixture'.\n"
    );
    assert_eq!(authority.revocations().len(), 2);
}

#[test]
fn mcp_logout_reports_a_failed_revocation_and_ignores_repairs() {
    let authority = Authority::shaped(Shape {
        revocation: "400 Bad Request",
        ..PLAIN
    });
    let home = Home::new(&authority.origin);
    let (granted, _, _) = authorize(&home, "fixture");
    assert!(granted.status.success());
    let mut stored: Value =
        serde_json::from_str(&fs::read_to_string(home.store()).expect("test step"))
            .expect("test step");
    stored["credentials"]
        .as_array_mut()
        .expect("test step")
        .push(json!({}));
    fs::write(home.store(), stored.to_string()).expect("test step");
    let output = logout(&home);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Logged out of MCP server 'fixture' locally; remote revocation failed.\n"
    );
    assert!(!home.store().exists());
    assert_secret_free(&home, &output);
}

#[test]
fn mcp_logout_of_an_unapproved_project_server_stays_local() {
    let authority = Authority::start();
    let home = Home::new(&authority.origin);
    let endpoint = format!("{}/mcp", authority.origin);
    fs::write(
        home.workspace().join(".mcp.json"),
        json!({"mcpServers": {"project": {"type": "http", "url": endpoint}}}).to_string(),
    )
    .expect("project servers");
    let directory = home.root.join("data/oh-fx/mcp-credentials");
    fs::create_dir_all(&directory).expect("test step");
    for path in [home.root.join("data/oh-fx"), directory.clone()] {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("test step");
    }
    fs::write(
        home.store(),
        json!({"version": 1, "credentials": [{
            "server_identity": "project",
            "endpoint": endpoint,
            "resource": endpoint,
            "issuer": authority.origin,
            "client_id": "registered-client",
            "client_secret": null,
            "access_token": ACCESS,
            "refresh_token": REFRESH,
            "scope": "",
            "token_type": "Bearer",
            "token_endpoint_auth_method": "none",
            "expires_at_ms": null,
            "authorization_endpoint": format!("{}/authorize", authority.origin),
            "token_endpoint": format!("{}/token", authority.origin),
            "revocation_endpoint": format!("{}/revoke", authority.origin),
        }]})
        .to_string(),
    )
    .expect("test step");
    fs::set_permissions(home.store(), fs::Permissions::from_mode(0o600)).expect("test step");
    let output = home
        .command(&["mcp", "logout", "project"])
        .output()
        .expect("test step");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "",
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Logged out of MCP server 'project' locally.\n"
    );
    assert!(!home.store().exists());
    assert!(authority.paths().is_empty());
}

#[test]
fn mcp_logout_refuses_unknown_and_local_servers_and_bad_usage() {
    let authority = Authority::start();
    let home = Home::new(&authority.origin);
    for (name, failure) in [
        ("absent", "oh-fx mcp logout failed: McpServerNotFound.\n"),
        (
            "local",
            "oh-fx mcp logout failed: McpAuthenticationNotRemote.\n",
        ),
    ] {
        let output = home
            .command(&["mcp", "logout", name])
            .output()
            .expect("test step");
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(String::from_utf8_lossy(&output.stderr), failure);
        assert!(output.stdout.is_empty());
    }
    for args in [
        &["mcp", "logout"][..],
        &["mcp", "logout", "a", "b"],
        &["mcp", "logout", ""],
    ] {
        let output = home.command(args).output().expect("test step");
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "usage: oh-fx mcp <command> ...\n",
            "{args:?}"
        );
    }
    assert!(authority.paths().is_empty());
}

#[test]
fn mcp_logout_in_the_shell_disconnects_until_auth_signs_in_again() {
    let authority = Authority::start();
    let home = Home::new(&authority.origin);
    let model = FakeServer::start([]);
    let session = home.shell(&model, &json!({}));
    settled_menu(
        &session,
        &server_row("fixture", "Needs authentication", "HTTP · Profile"),
    );
    session.send(b"/mcp auth fixture --open\r");
    let url = wait_for_file(&home.opened()).trim().to_owned();
    fs::remove_file(home.opened()).expect("test step");
    visit(&format!(
        "{}?code=browser-code&state={}",
        query(&url, "redirect_uri"),
        query(&url, "state")
    ));
    shown(&session, "Authenticated MCP server 'fixture'.");
    settled_menu(&session, &server_row("fixture", "Ready", "HTTP · Profile"));
    let mut stored: Value =
        serde_json::from_str(&fs::read_to_string(home.store()).expect("test step"))
            .expect("test step");
    stored["credentials"]
        .as_array_mut()
        .expect("test step")
        .push(json!({}));
    fs::write(home.store(), stored.to_string()).expect("test step");
    session.send(b"/mcp logout fixture\r");
    shown(
        &session,
        "Logged out of MCP server 'fixture'. Removed 1 unreadable MCP credential entry.",
    );
    menu_shows(
        &session,
        &server_row("fixture", "Disconnected", "HTTP · Profile"),
    );
    session.send(b"\r");
    shown(&session, "enter sign in  c help  esc back");
    session.send(b"\x1b");
    shown(&session, MENU_HINT);
    close_menu(&session);
    assert!(!home.store().exists());
    assert_eq!(authority.revocations().len(), 2);
    assert!(
        authority
            .closings()
            .iter()
            .any(|bearer| bearer.as_deref() == Some(format!("Bearer {ACCESS}").as_str()))
    );
    let before = authority.mcp_authorizations().len();
    session.send(b"/mcp auth fixture --open\r");
    let url = wait_for_file(&home.opened()).trim().to_owned();
    let page = visit(&format!(
        "{}?code=browser-code&state={}",
        query(&url, "redirect_uri"),
        query(&url, "state")
    ));
    assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
    let bearer = format!("Bearer {ACCESS}");
    let signed_in_again = |authority: &Authority| {
        authority.mcp_authorizations()[before..]
            .iter()
            .any(|sent| sent.as_deref() == Some(bearer.as_str()))
    };
    let started = Instant::now();
    while started.elapsed() < SHELL_WAIT && !(signed_in_again(&authority) && home.store().exists())
    {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(home.store().exists());
    assert!(signed_in_again(&authority));
    session
        .wait_for(SHELL_WAIT, |screen| {
            screen.rfind("Authenticated MCP server 'fixture'.")
                > screen.rfind("Logged out of MCP server 'fixture'.")
        })
        .unwrap_or_else(|screen| panic!("expected a second sign-in on screen:\n{screen}"));
    let screen = session.screen();
    for secret in [ACCESS, REFRESH, "browser-code", "token="] {
        assert!(!screen.contains(secret), "{screen}");
    }
}

#[test]
fn mcp_logout_in_the_shell_waits_for_a_pending_authentication() {
    let authority = Authority::start();
    let home = Home::new(&authority.origin);
    let model = FakeServer::start([]);
    let session = home.shell(&model, &json!({}));
    settled_menu(
        &session,
        &server_row("fixture", "Needs authentication", "HTTP · Profile"),
    );
    session.send(b"/mcp auth fixture --open\r");
    wait_for_file(&home.opened());
    session.send(b"/mcp logout fixture\r");
    shown(
        &session,
        "MCP authentication for 'fixture' is still in progress. Wait for it to finish before logging out.",
    );
    thread::sleep(Duration::from_millis(500));
    let screen = session.screen();
    assert!(
        !screen.contains("No stored MCP credentials found for 'fixture'."),
        "{screen}"
    );
    assert!(authority.revocations().is_empty());
}

#[test]
fn mcp_logout_reports_a_store_failure_and_traces_it_without_secrets() {
    let authority = Authority::start();
    let home = Home::new(&authority.origin);
    let (granted, _, _) = authorize(&home, "fixture");
    assert!(granted.status.success());
    fs::write(home.store(), "{").expect("test step");
    let output = logout(&home);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "oh-fx mcp logout failed: UnexpectedEndOfInput.\n"
    );
    assert!(output.stdout.is_empty());
    let trace = fs::read_to_string(home.root.join("trace.log")).expect("the trace log");
    assert!(
        trace.contains(
            "stored credential load skipped during logout server=fixture err=UnexpectedEndOfInput"
        ),
        "{trace}"
    );
    assert_secret_free(&home, &output);
    assert!(authority.revocations().is_empty());
}

#[test]
fn mcp_list_reports_a_stored_grant_without_connecting() {
    let authority = Authority::start();
    let home = Home::new(&authority.origin);
    let listed = |args: &[&str]| {
        let output = home.command(args).output().expect("test step");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("test step")
    };
    let before = listed(&["mcp", "list"]);
    assert!(
        before.contains(
            "fixture source=profile scope=profile policy=optional transport=http state=disconnected auth=none status=unavailable"
        ),
        "{before}"
    );
    let (output, _, _) = authorize(&home, "fixture");
    assert!(output.status.success());
    let requests = authority.paths().len();
    let after = listed(&["mcp", "list"]);
    assert!(
        after.contains(
            "fixture source=profile scope=profile policy=optional transport=http state=disconnected auth=authenticated status=unavailable"
        ),
        "{after}"
    );
    assert!(
        after.contains(
            "local source=profile scope=profile policy=optional transport=stdio state=disconnected auth=none status=unavailable"
        ),
        "{after}"
    );
    assert_eq!(authority.paths().len(), requests);
    let connected = listed(&["mcp", "list", "--connect"]);
    assert!(
        connected.contains(
            "fixture source=profile scope=profile policy=optional transport=http state=ready auth=authenticated status=ready"
        ),
        "{connected}"
    );
    for secret in [ACCESS, REFRESH] {
        assert!(!before.contains(secret) && !after.contains(secret) && !connected.contains(secret));
    }
    fs::write(home.store(), "{").expect("test step");
    let broken = home.command(&["mcp", "list"]).output().expect("test step");
    assert_eq!(broken.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&broken.stderr),
        "oh-fx mcp list failed: UnexpectedEndOfInput.\n"
    );
}
