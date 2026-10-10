use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, UNIX_EPOCH};

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

    fn session(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        let mut full = vec!["session"];
        full.extend_from_slice(args);
        self.run("workspace", &full, environment)
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

fn private(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set permissions");
}

fn save_in_fx(home: &Home, id: &str, workspace: &str, title: &str, prompts: &[&str]) {
    let fx = home.root.join(".fx");
    let session = fx.join("sessions").join(id);
    fs::create_dir_all(&session).expect("create an fx session");
    for directory in [&fx, &fx.join("sessions"), &session] {
        private(directory, 0o700);
    }
    let workspace = home.root.join(workspace).display().to_string();
    let manifest = format!(
        "{{\"schema_version\":4,\"id\":\"{id}\",\"origin_workspace_root\":\"{workspace}\",\"workspace_root\":\"{workspace}\",\"created_at_ms\":1,\"updated_at_ms\":2,\"conversation_language\":\"en\",\"provider\":\"gateway\",\"model\":\"openai/gpt-5\",\"effort\":\"auto\",\"fast_mode\":false,\"title\":\"{title}\",\"subagent_child\":false}}"
    );
    let mut events = String::new();
    for (turn, prompt) in (0_u64..).zip(prompts) {
        for (offset, event) in [
            (1, format!("{{\"user\":{{\"text\":\"{prompt}\",\"images\":[],\"work_id\":null}}}}")),
            (2, "{\"assistant\":{\"text\":\"done\",\"provider_replay\":null,\"standalone_response\":false}}".to_owned()),
            (3, "{\"turn_completed\":{\"files\":[],\"turn_summary\":null}}".to_owned()),
        ] {
            let _ = writeln!(
                events,
                "{{\"schema_version\":3,\"seq\":{},\"timestamp_ms\":2,\"event\":{event}}}",
                turn * 3 + offset
            );
        }
    }
    for (name, bytes) in [
        ("session.json", manifest),
        ("events.jsonl", events),
        ("session.lock", String::new()),
    ] {
        fs::write(session.join(name), bytes).expect("write an fx session file");
        private(&session.join(name), 0o600);
    }
    date_fx_log(&session.join("events.jsonl"));
}

fn date_fx_log(log: &Path) {
    fs::File::options()
        .write(true)
        .open(log)
        .and_then(|log| log.set_modified(UNIX_EPOCH + Duration::from_secs(100)))
        .expect("date the fx log");
}

fn fx_shell_turn(replay_ref: &str, replay_bytes: &str) -> Vec<String> {
    let result = format!(
        "{{\"tool_result\":{{\"call_id\":\"call-1\",\"tool_name\":\"shell\",\"status\":\"success\",\"artifact_ref\":\"result-shell-0011223344556677-8899aabbccddeeff.txt\",\"tool_image_handle\":null,\"output_bytes\":3,\"stored_bytes\":3,\"completeness\":\"complete\",\"preview\":\"a.txt\",\"provider_native\":false,\"created_at_ms\":2,\"permission_feedback\":[],\"committed_file_presentation\":null,\"command_replay_ref\":{replay_ref},\"command_replay_bytes\":{replay_bytes},\"command_process_presentation\":null,\"terminal_action_presentation\":null}}}}"
    );
    vec![
        "{\"user\":{\"text\":\"list files\",\"images\":[],\"work_id\":null}}".to_owned(),
        "{\"tool_call\":{\"call_id\":\"call-1\",\"tool_name\":\"shell\",\"arguments_json\":\"{\\\"command\\\":\\\"ls\\\"}\",\"argument_integrity\":\"valid\",\"provisional_id\":null,\"provider_result\":null,\"final_identity\":\"valid\",\"provenance\":\"fx_local\"}}".to_owned(),
        result,
        "{\"assistant\":{\"text\":\"done\",\"provider_replay\":null,\"standalone_response\":false}}".to_owned(),
        "{\"turn_completed\":{\"files\":[],\"turn_summary\":null}}".to_owned(),
    ]
}

fn rewrite_fx_log(home: &Home, id: &str, events: &[String]) {
    let log = home.root.join(".fx/sessions").join(id).join("events.jsonl");
    let mut frames = String::new();
    for (seq, event) in (1_u64..).zip(events) {
        let _ = writeln!(
            frames,
            "{{\"schema_version\":3,\"seq\":{seq},\"timestamp_ms\":2,\"event\":{event}}}"
        );
    }
    fs::write(&log, frames).expect("rewrite the fx log");
    date_fx_log(&log);
}

fn fx_tree(home: &Home) -> Vec<(PathBuf, u64, i64, i64, u32)> {
    let mut entries = Vec::new();
    let mut pending = vec![home.root.join(".fx")];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).expect("stat the fx tree");
        if metadata.is_dir() {
            for entry in fs::read_dir(&path).expect("read the fx tree") {
                pending.push(entry.expect("an fx entry").path());
            }
        }
        entries.push((
            path,
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.mode(),
        ));
    }
    entries.sort();
    entries
}

#[test]
fn sessions_fx_saved_are_listed_with_its_marker_and_left_untouched() {
    let server = FakeServer::start(replies(1));
    let home = Home::new(&server.base_url());
    home.ask("workspace", "kept here");
    save_in_fx(&home, "fx-here", "workspace", "From fx", &["one", "two"]);
    save_in_fx(
        &home,
        "fx-there",
        "elsewhere",
        "Elsewhere in fx",
        &["three"],
    );
    save_in_fx(&home, "fx-replayed", "workspace", "Replayed in fx", &[]);
    rewrite_fx_log(
        &home,
        "fx-replayed",
        &fx_shell_turn(
            "\"fx-command-replay-00112233445566778899aabbccddeeff\"",
            "64",
        ),
    );
    save_in_fx(
        &home,
        "fx-orphan-result",
        "workspace",
        "Unreadable in fx",
        &[],
    );
    let mut orphaned = fx_shell_turn("null", "null");
    orphaned.remove(1);
    rewrite_fx_log(&home, "fx-orphan-result", &orphaned);
    let before = fx_tree(&home);

    let stdout = home.sessions(&[]);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 7, "{stdout}");
    assert_eq!(lines[0], "[sessions] 3 saved");
    assert_eq!(lines[1], " - kept here");
    assert!(!lines[2].ends_with(" | fx"), "{stdout}");
    assert_eq!(lines[3], " - Replayed in fx");
    assert_eq!(
        lines[4],
        "   id=fx-replayed | 1 turn | English | updated 1970-01-01 00:01:40.000 UTC | fx"
    );
    assert_eq!(lines[5], " - From fx");
    assert_eq!(
        lines[6],
        "   id=fx-here | 2 turns | English | updated 1970-01-01 00:01:40.000 UTC | fx"
    );

    let listing = home.listed(&["--all"]);
    assert_eq!(listing["count"], 4);
    let sessions = listing["sessions"].as_array().expect("sessions");
    assert_eq!(sessions[0]["title"], "kept here");
    assert_eq!(sessions[0].get("source"), None);
    for (session, id) in sessions[1..]
        .iter()
        .zip(["fx-there", "fx-replayed", "fx-here"])
    {
        assert_eq!(session["id"], id);
        assert_eq!(session["source"], "fx");
    }
    assert_eq!(
        sessions[1]["workspace_root"],
        home.root.join("elsewhere").display().to_string().as_str()
    );
    assert!(!stdout.contains("fx-orphan-result"), "{stdout}");
    assert_eq!(fx_tree(&home), before);
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

fn described(output: &Output) -> String {
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), "");
    text(&output.stdout)
}

fn refused(output: &Output) -> (String, String) {
    assert_eq!(output.status.code(), Some(1));
    (text(&output.stdout), text(&output.stderr))
}

#[test]
fn session_last_describes_the_newest_listed_session_of_the_workspace() {
    let server = FakeServer::start(replies(3));
    let home = Home::new(&server.base_url());
    home.ask("workspace", "older");
    home.ask("workspace", "newer");
    home.ask("elsewhere", "newest elsewhere");
    save_in_fx(&home, "fx-older", "workspace", "Older in fx", &["one"]);
    let before = fx_tree(&home);
    let listing = home.listed(&[]);
    let newest = &listing["sessions"][0];
    assert_eq!(newest["title"], "newer");

    assert_eq!(
        described(&home.session(&["last"], &[])),
        format!(
            "[session] {}\ncreated_at_ms: {}\nupdated_at_ms: {}\nlanguage: {}\nhistory_len: 1\n",
            newest["id"].as_str().expect("an id"),
            newest["created_at_ms"],
            newest["updated_at_ms"],
            newest["conversation_language"]
                .as_str()
                .expect("a language"),
        )
    );
    let mut expected = serde_json::Map::new();
    expected.insert("kind".to_owned(), json!("session_summary"));
    expected.extend(newest.as_object().expect("a listed session").clone());
    assert_eq!(
        described(&home.session(&["last", "--json"], &[])),
        format!("{}\n", Value::Object(expected))
    );
    assert_eq!(
        described(&home.session(&["\tlast ", "--json"], &[])),
        described(&home.session(&["last", "--json"], &[]))
    );
    assert_eq!(fx_tree(&home), before);
}

#[test]
fn session_last_reaches_a_newer_session_fx_saved_and_marks_it() {
    let server = FakeServer::start(replies(1));
    let home = Home::new(&server.base_url());
    home.ask("elsewhere", "not here");
    save_in_fx(&home, "fx-here", "workspace", "From fx", &["one", "two"]);
    let before = fx_tree(&home);
    let workspace = home.root.join("workspace").display().to_string();

    assert_eq!(
        described(&home.session(&["last"], &[])),
        "[session] fx-here\ncreated_at_ms: 1\nupdated_at_ms: 100000\nlanguage: en\nhistory_len: 2\nsource: fx\n"
    );
    assert_eq!(
        described(&home.session(&["last", "--json"], &[])),
        format!(
            "{{\"kind\":\"session_summary\",\"id\":\"fx-here\",\"title\":\"From fx\",\"preview\":null,\"workspace_root\":\"{workspace}\",\"origin_workspace_root\":\"{workspace}\",\"created_at_ms\":1,\"updated_at_ms\":100000,\"history_len\":2,\"conversation_language\":\"en\",\"source\":\"fx\"}}\n"
        )
    );
    assert_eq!(fx_tree(&home), before);
}

#[test]
fn session_last_says_why_the_workspace_has_no_session_to_describe() {
    let server = FakeServer::start(replies(1));
    let home = Home::new(&server.base_url());
    let none = (
        "{\"kind\":\"session\",\"error\":\"no saved sessions for this workspace\",\"code\":\"NoSavedSessions\"}\n",
        "oh-fx session: no saved sessions for this workspace\n",
    );
    let check = |expected: (&str, &str)| {
        assert_eq!(
            refused(&home.session(&["last", "--json"], &[])),
            (expected.0.to_owned(), String::new())
        );
        assert_eq!(
            refused(&home.session(&["last"], &[])),
            (String::new(), expected.1.to_owned())
        );
    };
    check(none);
    home.ask("elsewhere", "another workspace");
    check(none);
    break_session(&home.root.join("data/oh-fx/sessions"), "broken-session");
    check((
        "{\"kind\":\"session\",\"error\":\"saved sessions are unreadable; run `oh-fx doctor` for recovery guidance\",\"code\":\"NoReadableSessions\"}\n",
        "oh-fx session: saved sessions are unreadable; run `oh-fx doctor` for recovery guidance\n",
    ));
}

#[test]
fn session_last_needs_home_and_refuses_the_v2_store() {
    let server = FakeServer::start(replies(0));
    let home = Home::new(&server.base_url());
    for (args, expected) in [
        (
            &["session", "last", "--json"][..],
            (
                "{\"kind\":\"session\",\"error\":\"HOME is not set\",\"code\":\"HomeNotSet\"}\n",
                "",
            ),
        ),
        (
            &["session", "last"],
            ("", "oh-fx session: HOME is not set\n"),
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
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
        assert_eq!(
            refused(&output),
            (expected.0.to_owned(), expected.1.to_owned()),
            "{args:?}"
        );
    }
    assert_eq!(
        refused(&home.session(&["last"], &[("OH_FX_SESSIONS_V2", "1")])),
        (
            String::new(),
            "oh-fx: session is not available yet\n".to_owned()
        )
    );
    let output = home.run(
        "workspace",
        &["--sessions-v2", "session", "last", "--json"],
        &[],
    );
    assert_eq!(
        refused(&output),
        (
            "{\"kind\":\"session\",\"error\":\"session is not available yet\",\"code\":\"NotAvailableYet\"}\n".to_owned(),
            "oh-fx: session is not available yet\n".to_owned()
        )
    );
}

#[test]
fn session_last_encodes_stored_text_for_the_terminal() {
    let server = FakeServer::start(replies(1));
    let home = Home::new(&server.base_url());
    home.ask("workspace", "own");
    let id = ids(&home.listed(&[]))[0].clone();
    let manifest = home
        .root
        .join("data/oh-fx/sessions")
        .join(&id)
        .join("session.json");
    let mut saved: Value =
        serde_json::from_slice(&fs::read(&manifest).expect("read session.json")).expect("JSON");
    saved["conversation_language"] = json!("en\u{9b}2J\u{202e}");
    fs::write(&manifest, saved.to_string()).expect("rewrite session.json");

    let text = described(&home.session(&["last"], &[]));
    assert!(
        text.contains("\nlanguage: en\\u{009b}2J\\u{202e}\n"),
        "{text}"
    );
    assert!(
        !text.contains('\u{9b}') && !text.contains('\u{202e}'),
        "{text}"
    );
    let json: Value =
        serde_json::from_str(&described(&home.session(&["last", "--json"], &[]))).expect("JSON");
    assert_eq!(json["conversation_language"], "en\u{9b}2J\u{202e}");
}

fn saved_metadata(home: &Home, id: &str) -> Value {
    let path = home
        .root
        .join("data/oh-fx/sessions")
        .join(id)
        .join("session.json");
    serde_json::from_slice(&fs::read(path).expect("read session.json")).expect("a manifest")
}

#[test]
fn session_shows_every_saved_turn_of_a_session_by_id() {
    let server = FakeServer::start(replies(1));
    let home = Home::new(&server.base_url());
    home.ask("elsewhere", "Fix the parser");
    let listing = home.listed(&["--all"]);
    let id = listing["sessions"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let saved = saved_metadata(&home, &id);
    let (created, updated, language) = (
        &saved["created_at_ms"],
        &saved["updated_at_ms"],
        saved["conversation_language"].as_str().expect("a language"),
    );

    let text = format!(
        "[session] {id}\ncreated_at_ms: {created}\nupdated_at_ms: {updated}\nlanguage: {language}\nhistory_len: 1\n\n[turn 1]\n[user]\nFix the parser\n[assistant]\ndone\n"
    );
    let json = format!(
        "{{\"kind\":\"session_detail\",\"id\":\"{id}\",\"created_at_ms\":{created},\"updated_at_ms\":{updated},\"history_len\":1,\"conversation_language\":\"{language}\",\"history\":[{{\"kind\":\"assistant\",\"user\":{{\"text\":\"Fix the parser\",\"images\":[]}},\"assistant\":\"done\",\"execution\":{{\"schema_version\":3,\"tool_steps\":[],\"files\":[],\"steering\":[]}}}}]}}\n"
    );
    let padded = format!(" {id}\t");
    for (args, expected) in [
        (vec![id.as_str()], &text),
        (vec!["--id", id.as_str()], &text),
        (vec![padded.as_str()], &text),
        (vec![id.as_str(), "--json"], &json),
        (vec!["--json", "--id", id.as_str()], &json),
    ] {
        assert_eq!(&described(&home.session(&args, &[])), expected, "{args:?}");
    }
}

#[test]
fn session_says_why_a_session_cannot_be_shown() {
    let server = FakeServer::start(replies(1));
    let home = Home::new(&server.base_url());
    home.ask("workspace", "kept");
    let sessions = home.root.join("data/oh-fx/sessions");
    break_session(&sessions, "broken-session");
    fs::create_dir_all(sessions.join("future")).expect("create a session directory");
    fs::write(
        sessions.join("future/session.json"),
        "{\"schema_version\":99}",
    )
    .expect("write a future session");
    for (id, code, message) in [
        ("missing", "SessionNotFound", "record not found".to_owned()),
        ("../x", "InvalidSessionId", "invalid session id".to_owned()),
        (
            "broken-session",
            "InvalidSessionFormat",
            "session broken-session is corrupt; run `oh-fx session recover broken-session`"
                .to_owned(),
        ),
        (
            "future",
            "UnsupportedSessionSchema",
            "session future uses an unsupported session version".to_owned(),
        ),
    ] {
        assert_eq!(
            refused(&home.session(&[id], &[])),
            (String::new(), format!("oh-fx session: {message}\n")),
            "{id}"
        );
        assert_eq!(
            refused(&home.session(&["--id", id, "--json"], &[])),
            (
                format!(
                    "{}\n",
                    json!({"kind": "session", "error": message, "code": code})
                ),
                String::new()
            ),
            "{id}"
        );
    }
    assert_eq!(
        refused(&home.session(&["missing"], &[("OH_FX_SESSIONS_V2", "true")])),
        (
            String::new(),
            "oh-fx: session is not available yet\n".to_owned()
        )
    );
}

#[test]
fn session_shows_a_session_fx_saved_without_touching_it() {
    let server = FakeServer::start(replies(1));
    let home = Home::new(&server.base_url());
    home.ask("workspace", "own");
    save_in_fx(&home, "fx-here", "workspace", "From fx", &["one", "two"]);
    save_in_fx(&home, "fx-orphan-result", "workspace", "Unreadable", &[]);
    let mut orphaned = fx_shell_turn("null", "null");
    orphaned.remove(1);
    rewrite_fx_log(&home, "fx-orphan-result", &orphaned);
    let before = fx_tree(&home);

    assert_eq!(
        described(&home.session(&["fx-here"], &[])),
        "[session] fx-here\ncreated_at_ms: 1\nupdated_at_ms: 2\nlanguage: en\nhistory_len: 2\nsource: fx\n\n[turn 1]\n[user]\none\n[assistant]\ndone\n\n[turn 2]\n[user]\ntwo\n[assistant]\ndone\n"
    );
    let json: Value = serde_json::from_str(&described(&home.session(&["fx-here", "--json"], &[])))
        .expect("a JSON detail");
    assert_eq!(json["kind"], "session_detail");
    assert_eq!(json["history_len"], 2);
    assert_eq!(json["source"], "fx");
    assert_eq!(
        refused(&home.session(&["fx-orphan-result"], &[])),
        (
            String::new(),
            "oh-fx session: this fx session holds data oh-fx cannot read yet; keep using it in fx\n"
                .to_owned()
        )
    );
    assert_eq!(fx_tree(&home), before);
}
