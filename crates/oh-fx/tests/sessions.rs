use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use ofx_testkit::{FakeServer, Reply, chat_text_events};
use serde_json::{Value, json};

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new(base_url: &str) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = fs::canonicalize(directory.path()).expect("canonicalize the home");
        for name in ["config/oh-fx", "workspace", "elsewhere"] {
            fs::create_dir_all(root.join(name)).expect("create a directory");
        }
        let settings = json!({
            "provider": "local",
            "model": "local-model",
            "providers": {"local": {
                "protocol": "openai-chat-completions",
                "base_url": base_url,
                "auth": {"type": "none"},
                "models": ["local-model"],
            }},
        });
        fs::write(
            root.join("config/oh-fx/settings.json"),
            settings.to_string(),
        )
        .expect("write settings");
        Self {
            _directory: directory,
            root,
        }
    }

    fn run(&self, directory: &str, args: &[&str], environment: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
            .current_dir(self.root.join(directory))
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .envs(environment.iter().copied())
            .stdin(Stdio::null())
            .output()
            .expect("run oh-fx")
    }

    fn ask(&self, directory: &str, prompt: &str) {
        let output = self.run(directory, &["ask", prompt], &[]);
        assert!(output.status.success(), "{}", text(&output.stderr));
    }

    fn sessions(&self, args: &[&str]) -> String {
        let mut full = vec!["sessions"];
        full.extend_from_slice(args);
        let output = self.run("workspace", &full, &[]);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            text(&output.stderr)
        );
        assert_eq!(text(&output.stderr), "", "{args:?}");
        text(&output.stdout)
    }

    fn listed(&self, args: &[&str]) -> Value {
        let mut full = args.to_vec();
        full.push("--json");
        serde_json::from_str(&self.sessions(&full)).expect("a JSON listing")
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn replies(count: usize) -> Vec<Reply> {
    (0..count)
        .map(|_| Reply::sse(&chat_text_events(&["done"])))
        .collect()
}

fn ids(listing: &Value) -> Vec<String> {
    listing["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|session| session["id"].as_str().expect("an id").to_owned())
        .collect()
}

#[test]
fn an_empty_profile_has_no_saved_sessions() {
    let server = FakeServer::start(replies(0));
    let home = Home::new(&server.base_url());
    assert_eq!(home.sessions(&[]), "[sessions] no saved sessions\n");
    assert_eq!(
        home.sessions(&["--json"]),
        "{\"kind\":\"sessions\",\"count\":0,\"sessions\":[]}\n"
    );
}

#[test]
fn sessions_are_listed_newest_first_with_their_titles_and_details() {
    let server = FakeServer::start(replies(2));
    let home = Home::new(&server.base_url());
    home.ask("workspace", "Fix the parser\nsecond line");
    home.ask("workspace", "Write the docs");
    let listing = home.listed(&[]);
    assert_eq!(listing["kind"], "sessions");
    assert_eq!(listing["count"], 2);
    let sessions = listing["sessions"].as_array().expect("sessions");
    assert_eq!(sessions[0]["title"], "Write the docs");
    assert_eq!(sessions[1]["title"], "Fix the parser");
    let workspace = home.root.join("workspace").display().to_string();
    for session in sessions {
        assert_eq!(session["preview"], Value::Null);
        assert_eq!(session["workspace_root"], workspace.as_str());
        assert_eq!(session["origin_workspace_root"], workspace.as_str());
        assert_eq!(session["history_len"], 1);
        let keys: Vec<&str> = session
            .as_object()
            .expect("a session object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "id",
                "title",
                "preview",
                "workspace_root",
                "origin_workspace_root",
                "created_at_ms",
                "updated_at_ms",
                "history_len",
                "conversation_language"
            ]
        );
    }

    let stdout = home.sessions(&[]);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 5, "{stdout}");
    assert_eq!(lines[0], "[sessions] 2 saved");
    assert_eq!(lines[1], " - Write the docs");
    assert_eq!(lines[3], " - Fix the parser");
    for (line, session) in [(lines[2], &sessions[0]), (lines[4], &sessions[1])] {
        let id = session["id"].as_str().expect("an id");
        assert!(
            line.starts_with(&format!("   id={id} | 1 turn | ")),
            "{line}"
        );
        assert!(
            line.contains(" | updated ") && line.ends_with(" UTC"),
            "{line}"
        );
    }
}

#[test]
fn pages_continue_from_the_printed_cursor() {
    let server = FakeServer::start(replies(3));
    let home = Home::new(&server.base_url());
    for prompt in ["one", "two", "three"] {
        home.ask("workspace", prompt);
    }
    let all = ids(&home.listed(&[]));
    assert_eq!(all.len(), 3);
    let first = home.listed(&["--limit", "2"]);
    assert_eq!(ids(&first), all[..2]);
    assert_eq!(first["has_more"], true);
    let cursor = first["next_cursor"].as_str().expect("a cursor").to_owned();
    let last = &all[1];
    assert!(
        cursor.starts_with("v1:") && cursor.ends_with(&format!(":{last}")),
        "{cursor}"
    );
    let text_page = home.sessions(&["--limit", "2"]);
    assert!(
        text_page.ends_with(&format!(
            "[sessions] more saved sessions; continue with `oh-fx sessions --cursor {cursor}`\n"
        )),
        "{text_page}"
    );
    let rest = home.listed(&["--limit", "2", "--cursor", &cursor]);
    assert_eq!(ids(&rest), all[2..]);
    assert_eq!(rest.get("has_more"), None);
}

#[test]
fn other_workspaces_appear_only_with_all() {
    let server = FakeServer::start(replies(2));
    let home = Home::new(&server.base_url());
    home.ask("workspace", "here");
    home.ask("elsewhere", "there");
    assert_eq!(home.listed(&[])["count"], 1);
    let everywhere = home.listed(&["--all"]);
    assert_eq!(everywhere["count"], 2);
    let page = home.sessions(&["--all", "--limit", "1"]);
    assert!(
        page.contains("continue with `oh-fx sessions --all --cursor v1:"),
        "{page}"
    );
}

fn break_session(sessions: &Path, id: &str) {
    let session = sessions.join(id);
    fs::create_dir_all(&session).expect("create a session directory");
    fs::write(session.join("session.json"), "{").expect("write a broken session");
}

#[test]
fn unreadable_sessions_are_counted_and_reported() {
    let server = FakeServer::start(replies(1));
    let home = Home::new(&server.base_url());
    let sessions = home.root.join("data/oh-fx/sessions");
    home.ask("workspace", "kept");
    break_session(&sessions, "broken-session");
    let stdout = home.sessions(&[]);
    assert!(
        stdout.starts_with("[sessions] 1 saved\n - kept\n"),
        "{stdout}"
    );
    assert!(
        stdout.ends_with("[sessions] warning: skipped 1 unreadable saved session; run `oh-fx doctor` for recovery guidance\n"),
        "{stdout}"
    );
    assert_eq!(home.listed(&[])["skipped_invalid"], 1);

    let empty = FakeServer::start(replies(0));
    let home = Home::new(&empty.base_url());
    let sessions = home.root.join("data/oh-fx/sessions");
    fs::create_dir_all(&sessions).expect("create the sessions directory");
    for directory in [home.root.join("data/oh-fx"), sessions.clone()] {
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("make the directory private");
    }
    break_session(&sessions, "broken-a");
    break_session(&sessions, "broken-b");
    assert_eq!(
        home.sessions(&[]),
        "[sessions] no readable saved sessions\n[sessions] warning: skipped 2 unreadable saved sessions; run `oh-fx doctor` for recovery guidance\n"
    );
    assert_eq!(
        home.sessions(&["--json"]),
        "{\"kind\":\"sessions\",\"count\":0,\"skipped_invalid\":2,\"sessions\":[]}\n"
    );
}

#[test]
fn listing_needs_home_and_refuses_the_v2_store() {
    let server = FakeServer::start(replies(0));
    let home = Home::new(&server.base_url());
    let output = Command::new(env!("CARGO_BIN_EXE_oh-fx"))
        .args(["sessions", "--json"])
        .current_dir(home.root.join("workspace"))
        .env_clear()
        .env("XDG_CONFIG_HOME", home.root.join("config"))
        .env("XDG_STATE_HOME", home.root.join("state"))
        .env("XDG_DATA_HOME", home.root.join("data"))
        .env("XDG_CACHE_HOME", home.root.join("cache"))
        .env("OH_FX_AUTO_UPGRADE", "0")
        .stdin(Stdio::null())
        .output()
        .expect("run oh-fx");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stdout),
        "{\"kind\":\"sessions\",\"error\":\"HOME is not set\",\"code\":\"HomeNotSet\"}\n"
    );
    let output = home.run("workspace", &["sessions"], &[("OH_FX_SESSIONS_V2", "1")]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "oh-fx: sessions is not available yet\n"
    );
}
