use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use ofx_testkit::{FakeServer, Reply, chat_text_events};
use serde_json::{Value, json};

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../ofx-session/tests/fixtures/sessions_v2"
);
const LISTED: [&str; 4] = [
    "hDoX4WdPFWDK",
    "pgdu6JEc1UOO",
    "U9K-4ZzltsUh",
    "nJI52ukeF2bU",
];

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

    fn run(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
            .current_dir(self.root.join("workspace"))
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

    fn listed(&self, args: &[&str], environment: &[(&str, &str)]) -> String {
        let output = self.run(args, environment);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            text(&output.stderr)
        );
        assert_eq!(text(&output.stderr), "", "{args:?}");
        text(&output.stdout)
    }

    fn workspace(&self) -> String {
        self.root
            .join("workspace")
            .to_str()
            .expect("utf-8 path")
            .to_owned()
    }

    fn fx_root(&self) -> PathBuf {
        self.root.join(".fx/sessions/v2")
    }

    fn own_root(&self) -> PathBuf {
        self.root.join("data/oh-fx/sessions/v2")
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("utf-8 output")
}

fn fixture(name: &str) -> Vec<u8> {
    fs::read(Path::new(FIXTURES).join("listing").join(name)).expect("read a fixture")
}

fn private_root(root: &Path, levels: usize) {
    fs::create_dir_all(root).expect("create the v2 root");
    let mut folder = root.to_path_buf();
    for _ in 0..levels {
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).expect("make it private");
        folder.pop();
    }
}

fn install(root: &Path, levels: usize, index: &[u8], sessions: &[&str]) {
    private_root(root, levels);
    fs::write(root.join("index.jsonl"), index).expect("write the index");
    for id in sessions {
        fs::create_dir(root.join(id)).expect("create a session folder");
        fs::set_permissions(root.join(id), fs::Permissions::from_mode(0o700)).expect("chmod");
    }
}

fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0x82F6_3B78
            };
        }
    }
    !crc
}

fn put(id: &str, workspace: &str, title: &str, updated_ms: impl std::fmt::Display) -> String {
    let payload = format!(
        "{{\"op\":\"put\",\"id\":\"{id}\",\"role\":\"root\",\"host\":\"ask\",\"workspace\":\"{workspace}\",\"title\":\"{title}\",\"language\":\"und-Latn\",\"created\":{updated_ms},\"updated\":{updated_ms},\"turns\":1,\"opened\":[0,0,0,0,0]"
    );
    format!(
        "{payload},\"crc\":\"{:08x}\"}}\n",
        crc32c(payload.as_bytes())
    )
}

fn marked_json(fx: &str) -> String {
    fx.replace(
        "\"conversation_language\":\"und-Latn\"}",
        "\"conversation_language\":\"und-Latn\",\"source\":\"fx\"}",
    )
    .replace(
        "\"conversation_language\":\"und-Cyrl\"}",
        "\"conversation_language\":\"und-Cyrl\",\"source\":\"fx\"}",
    )
}

fn ids(listed: &str) -> Vec<String> {
    let listed: Value = serde_json::from_str(listed).expect("listing json");
    listed["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|session| session["id"].as_str().expect("id").to_owned())
        .collect()
}

#[test]
fn the_sessions_fx_saved_on_v2_list_as_fx_lists_them() {
    let home = Home::new("http://127.0.0.1:9");
    install(&home.fx_root(), 3, &fixture("index.jsonl"), &LISTED);
    let expected = marked_json(&text(&fixture("fx-sessions-all.json")));
    assert_eq!(
        home.listed(&["--sessions-v2", "sessions", "--all", "--json"], &[]),
        expected
    );
    assert_eq!(
        home.listed(
            &["sessions", "--all", "--json"],
            &[("OH_FX_SESSIONS_V2", "true")]
        ),
        expected
    );
    let fx_text = text(&fixture("fx-sessions-all.txt"));
    let marked: String = fx_text
        .lines()
        .map(|line| {
            if line.starts_with("   id=") {
                format!("{line} | fx\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    assert_eq!(
        home.listed(&["--sessions-v2", "sessions", "--all"], &[]),
        marked
    );
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..LISTED.len() {
        let mut args = vec![
            "--sessions-v2",
            "sessions",
            "--all",
            "--limit",
            "1",
            "--json",
        ];
        if let Some(cursor) = &cursor {
            args.extend(["--cursor", cursor.as_str()]);
        }
        let page: Value = serde_json::from_str(&home.listed(&args, &[])).expect("page json");
        seen.extend(ids(&page.to_string()));
        cursor = page["next_cursor"].as_str().map(str::to_owned);
    }
    assert_eq!(seen, LISTED);
    assert_eq!(cursor, None);
    assert!(!home.own_root().exists());
}

#[test]
fn each_store_lists_only_its_own_sessions() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["saved on v1"]))]);
    let home = Home::new(&server.base_url());
    let output = home.run(&["ask", "A v1 question."], &[]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    install(&home.fx_root(), 3, &fixture("index.jsonl"), &LISTED);
    assert_eq!(
        ids(&home.listed(&["--sessions-v2", "sessions", "--all", "--json"], &[])),
        LISTED
    );
    let v1 = ids(&home.listed(&["sessions", "--all", "--json"], &[]));
    assert_eq!(v1.len(), 1);
    assert!(!LISTED.contains(&v1[0].as_str()));
}

#[test]
fn session_last_on_v2_describes_the_newest_session_of_the_workspace() {
    let home = Home::new("http://127.0.0.1:9");
    let workspace = home.workspace();
    let elsewhere = home
        .root
        .join("elsewhere")
        .to_str()
        .expect("utf-8 path")
        .to_owned();
    let none = home.run(&["--sessions-v2", "session", "last"], &[]);
    assert_eq!(none.status.code(), Some(1));
    assert_eq!(
        text(&none.stderr),
        "oh-fx session: no saved sessions for this workspace\n"
    );
    let index = [
        put("fxOlderAaaa1", &workspace, "Older here.", 10),
        put("fxNewerAaaa2", &workspace, "Newer here.", 20),
        put("fxElsewhere3", &elsewhere, "Newest elsewhere.", 30),
    ]
    .concat();
    install(
        &home.fx_root(),
        3,
        index.as_bytes(),
        &["fxOlderAaaa1", "fxNewerAaaa2", "fxElsewhere3"],
    );
    assert_eq!(
        home.listed(&["--sessions-v2", "session", "last", "--json"], &[]),
        format!(
            "{{\"kind\":\"session_summary\",\"id\":\"fxNewerAaaa2\",\"title\":\"Newer here.\",\"preview\":null,\"workspace_root\":\"{workspace}\",\"origin_workspace_root\":\"{workspace}\",\"created_at_ms\":20,\"updated_at_ms\":20,\"history_len\":1,\"conversation_language\":\"und-Latn\",\"source\":\"fx\"}}\n"
        )
    );
    install(
        &home.own_root(),
        2,
        put("ownLatestId1", &workspace, "Mine.", 25).as_bytes(),
        &["ownLatestId1"],
    );
    assert_eq!(
        home.listed(&["--sessions-v2", "session", "last"], &[]),
        "[session] ownLatestId1\ncreated_at_ms: 25\nupdated_at_ms: 25\nlanguage: und-Latn\nhistory_len: 1\n"
    );
}

#[test]
fn a_put_with_an_out_of_range_number_is_skipped_and_an_underflow_reads_as_zero() {
    let home = Home::new("http://127.0.0.1:9");
    let workspace = home.workspace();
    let index = [
        put("fxHugeNumbr1", &workspace, "Huge.", "1e9223372036854775807"),
        put(
            "fxTinyNumbr2",
            &workspace,
            "Tiny.",
            "1.0e-9223372036854775807",
        ),
        put("fxPlainOne03", &workspace, "Plain.", 5),
    ]
    .concat();
    install(
        &home.fx_root(),
        3,
        index.as_bytes(),
        &["fxHugeNumbr1", "fxTinyNumbr2", "fxPlainOne03"],
    );
    assert_eq!(
        ids(&home.listed(&["--sessions-v2", "sessions", "--all", "--json"], &[])),
        ["fxPlainOne03", "fxTinyNumbr2"]
    );
    assert!(
        home.listed(&["--sessions-v2", "session", "last", "--json"], &[])
            .contains("\"id\":\"fxPlainOne03\"")
    );
}

#[test]
fn a_v2_index_that_cannot_be_read_fails_the_listing() {
    let home = Home::new("http://127.0.0.1:9");
    let workspace = home.workspace();
    install(
        &home.own_root(),
        2,
        put("ownSession01", &workspace, "Mine.", 5).as_bytes(),
        &["ownSession01"],
    );
    let index = home.own_root().join("index.jsonl");
    fs::set_permissions(&index, fs::Permissions::from_mode(0o000)).expect("chmod");
    if fs::read(&index).is_ok() {
        return;
    }
    let output = home.run(&["--sessions-v2", "sessions", "--json"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stdout),
        "{\"kind\":\"sessions\",\"error\":\"durable session store is unavailable\",\"code\":\"SessionStoreUnavailable\"}\n"
    );
    let output = home.run(&["--sessions-v2", "session", "last"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "oh-fx session: durable session store is unavailable\n"
    );
}

fn copy_fixture(scenario: &str, root: &Path) {
    copy_tree(&Path::new(FIXTURES).join(scenario), root);
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create a fixture folder");
    fs::set_permissions(to, fs::Permissions::from_mode(0o700)).expect("chmod");
    for entry in fs::read_dir(from).expect("read a fixture folder") {
        let entry = entry.expect("a fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("a file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "json" && extension != "txt")
        {
            fs::copy(entry.path(), &target).expect("copy a fixture file");
            let mode = if from.ends_with("blobs") {
                0o400
            } else {
                0o600
            };
            fs::set_permissions(&target, fs::Permissions::from_mode(mode)).expect("chmod");
        }
    }
}

fn doctor_lines(home: &Home, environment: &[(&str, &str)]) -> Vec<String> {
    let output = home.run(&["--sessions-v2", "doctor"], environment);
    assert!(output.status.success(), "{}", text(&output.stderr));
    text(&output.stdout)
        .lines()
        .filter(|line| {
            line.split_once("] ").is_some_and(|(_, check)| {
                ["state:", "session:", "sessions:"]
                    .iter()
                    .any(|name| check.starts_with(name))
            })
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn doctor_on_v2_reports_its_sessions_as_fx_reports_them() {
    let home = Home::new("http://127.0.0.1:9");
    assert_eq!(
        doctor_lines(&home, &[]),
        [
            "[ok] state: sessions v2",
            "[warn] sessions: no saved sessions yet"
        ]
    );
    copy_fixture("snapshot_small", &home.own_root());
    private_root(&home.own_root(), 2);
    assert_eq!(
        doctor_lines(&home, &[]),
        [
            "[ok] state: sessions v2",
            "[ok] sessions: 1 saved session(s); latest=PsjMeU3Vl5Ld"
        ]
    );
}

#[test]
fn doctor_on_v2_warns_about_a_damaged_or_unreadable_log_and_traces_it() {
    let home = Home::new("http://127.0.0.1:9");
    copy_fixture("listing", &home.own_root());
    private_root(&home.own_root(), 2);
    let log = home.own_root().join("nJI52ukeF2bU/log.jsonl");
    let mut bytes = fs::read(&log).expect("read a log");
    let at = bytes
        .iter()
        .position(|&byte| byte == b'\n')
        .expect("a line")
        + 20;
    bytes[at] ^= 0x20;
    fs::write(&log, bytes).expect("damage the log");
    let unreadable = home.own_root().join("U9K-4ZzltsUh/log.jsonl");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).expect("chmod");
    let readable = fs::read(&unreadable).is_ok();
    let trace = home.root.join("trace.log");
    let trace_path = trace.to_str().expect("utf-8 path").to_owned();
    let lines = doctor_lines(&home, &[("OH_FX_TRACE_LOG", &trace_path)]);
    let mut expected = vec!["[ok] state: sessions v2".to_owned()];
    if !readable {
        expected.push("[warn] session: session U9K-4ZzltsUh has a damaged log; `oh-fx session recover U9K-4ZzltsUh` copies its good turns".to_owned());
    }
    expected.push("[warn] session: session nJI52ukeF2bU has a damaged log; `oh-fx session recover nJI52ukeF2bU` copies its good turns".to_owned());
    expected.push("[ok] sessions: 4 saved session(s); latest=hDoX4WdPFWDK".to_owned());
    assert_eq!(lines, expected);
    let written = fs::read_to_string(&trace).expect("read the trace");
    assert!(
        written.contains(" [wiring] action=StoreOpen session=- pid="),
        "{written}"
    );
    assert!(
        written.contains(" [wiring] action=StoreClose session=- pid="),
        "{written}"
    );
    if !readable {
        assert!(
            written.contains(" [session] event=sessions_v2_doctor_unreadable session=U9K-4ZzltsUh err=AccessDenied\n"),
            "{written}"
        );
    }
}

const DETAILED: [(&str, &str); 4] = [
    ("listing", "nJI52ukeF2bU"),
    ("listing", "hDoX4WdPFWDK"),
    ("tool", "ia8mVXaiyAf1"),
    ("kill_tool", "NYXx_508J28b"),
];

fn fx_detail(scenario: &str, id: &str, extension: &str) -> String {
    let text = fs::read_to_string(
        Path::new(FIXTURES)
            .join(scenario)
            .join(format!("fx-session-{id}.{extension}")),
    )
    .expect("read fx's detail");
    text.replace(
        "\"output\":\"fx stopped while this tool was running, so it may have partly run. Check its effects before running it again.\",\"output_bytes\":109,\"stored_output_bytes\":109",
        "\"output\":\"oh-fx stopped while this tool was running, so it may have partly run. Check its effects before running it again.\",\"output_bytes\":112,\"stored_output_bytes\":112",
    )
}

#[test]
fn session_detail_on_v2_equals_fx_s_for_the_sessions_fx_wrote() {
    let home = Home::new("http://127.0.0.1:9");
    for (scenario, _) in DETAILED {
        copy_fixture(scenario, &home.own_root());
    }
    private_root(&home.own_root(), 2);
    for (scenario, id) in DETAILED {
        assert_eq!(
            home.listed(&["--sessions-v2", "session", id, "--json"], &[]),
            fx_detail(scenario, id, "json"),
            "{id}"
        );
    }
    assert_eq!(
        home.listed(&["--sessions-v2", "session", "nJI52ukeF2bU"], &[]),
        fx_detail("listing", "nJI52ukeF2bU", "txt")
    );
}

#[test]
fn session_detail_on_v2_reads_fx_s_own_v2_sessions_marked_fx() {
    let home = Home::new("http://127.0.0.1:9");
    copy_fixture("tool", &home.fx_root());
    private_root(&home.fx_root(), 3);
    let expected = fx_detail("tool", "ia8mVXaiyAf1", "json");
    let marked = format!(
        "{},\"source\":\"fx\"}}\n",
        expected.trim_end().trim_end_matches('}')
    );
    assert_eq!(
        home.listed(&["--sessions-v2", "session", "ia8mVXaiyAf1", "--json"], &[]),
        marked
    );
}

#[test]
fn session_detail_on_v2_fails_as_upstream_does() {
    let home = Home::new("http://127.0.0.1:9");
    copy_fixture("listing", &home.own_root());
    private_root(&home.own_root(), 2);
    let missing = home.run(
        &["--sessions-v2", "session", "NoSuchSession1", "--json"],
        &[],
    );
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        text(&missing.stdout),
        "{\"kind\":\"session\",\"error\":\"record not found\",\"code\":\"SessionNotFound\"}\n"
    );
    let log = home.own_root().join("nJI52ukeF2bU/log.jsonl");
    let mut bytes = fs::read(&log).expect("read a log");
    let end = bytes.len();
    bytes[end - 5] ^= 1;
    fs::write(&log, bytes).expect("damage the log");
    let damaged = home.run(&["--sessions-v2", "session", "nJI52ukeF2bU"], &[]);
    assert_eq!(damaged.status.code(), Some(1));
    assert_eq!(
        text(&damaged.stderr),
        "oh-fx session: session nJI52ukeF2bU is corrupt; run `oh-fx session recover nJI52ukeF2bU`\n"
    );
    copy_fixture("tool", &home.fx_root());
    private_root(&home.fx_root(), 3);
    let fx_log = home.fx_root().join("ia8mVXaiyAf1/log.jsonl");
    let mut bytes = fs::read(&fx_log).expect("read fx's log");
    let end = bytes.len();
    bytes[end - 5] ^= 1;
    fs::write(&fx_log, bytes).expect("damage fx's log");
    let unreadable = home.run(&["--sessions-v2", "session", "ia8mVXaiyAf1"], &[]);
    assert_eq!(unreadable.status.code(), Some(1));
    assert_eq!(
        text(&unreadable.stderr),
        "oh-fx session: this fx session holds data oh-fx cannot read yet; keep using it in fx\n"
    );
}

fn logged(root: &Path, id: &str) -> Vec<Value> {
    let bytes = fs::read(root.join(id).join("log.jsonl")).expect("read the log");
    let mut lines = Vec::new();
    for (seq, line) in (1_u64..).zip(bytes.split_inclusive(|&byte| byte == b'\n')) {
        let text = std::str::from_utf8(line).expect("utf-8 line");
        let (payload, crc) = text
            .strip_suffix("\"}\n")
            .and_then(|rest| rest.rsplit_once(",\"crc\":\""))
            .expect("a framed line");
        assert_eq!(crc, format!("{:08x}", crc32c(payload.as_bytes())), "{text}");
        let line: Value = serde_json::from_str(text).expect("a JSON line");
        assert_eq!(line["seq"], seq, "{text}");
        lines.push(line);
    }
    lines
}

fn kinds(lines: &[Value]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            let kind = line["kind"].as_str().expect("kind");
            match line["type"].as_str().or_else(|| line["key"].as_str()) {
                Some(detail) => format!("{kind}:{detail}"),
                None => kind.to_owned(),
            }
        })
        .collect()
}

#[test]
fn ask_on_v2_saves_a_new_session_that_lists_and_reads_back_and_never_writes_v1() {
    let server = FakeServer::start([
        Reply::sse(&ofx_testkit::chat_tool_call_events(
            "call_1",
            "read_file",
            &json!({"path": "notes.txt"}).to_string(),
        )),
        Reply::sse(&chat_text_events(&["Read it."])),
    ]);
    let home = Home::new(&server.base_url());
    fs::write(home.root.join("workspace/notes.txt"), "NOTE_TEXT\n").expect("write notes");
    let trace = home.root.join("trace.log");
    let output = home.run(
        &["--sessions-v2", "ask", "--json", "Read the notes."],
        &[("OH_FX_TRACE_LOG", trace.to_str().expect("utf-8 path"))],
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    let result: Value = serde_json::from_slice(&output.stdout).expect("ask json");
    let id = result["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    assert_eq!(id.len(), 12);
    assert!(result.get("recovery").is_none());
    let lines = logged(&home.own_root(), &id);
    assert_eq!(
        kinds(&lines),
        [
            "session_created",
            "set:prefs",
            "set:permissions",
            "set:language",
            "turn_started",
            "item:user",
            "item:tool_call",
            "item:tool_result",
            "item:assistant",
            "item:turn_end",
            "turn_committed",
            "set:title",
            "closed"
        ]
    );
    assert_eq!(lines[0]["workspace"], home.workspace());
    assert_eq!(lines[0]["host"], "ask");
    let hash = lines[5]["blobs"][0].as_str().expect("a listed blob");
    assert_eq!(
        lines[7]["data"]["artifact_ref"],
        format!("result-{hash}.txt")
    );
    assert!(
        fs::read_to_string(home.own_root().join(&id).join("blobs").join(hash))
            .expect("the result blob")
            .contains("NOTE_TEXT")
    );
    let v1: Vec<_> = fs::read_dir(home.root.join("data/oh-fx/sessions"))
        .expect("the sessions folder")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(v1, ["v2"]);
    let listed: Value =
        serde_json::from_str(&home.listed(&["--sessions-v2", "sessions", "--json"], &[]))
            .expect("listing json");
    assert_eq!(listed["sessions"][0]["id"], id.as_str());
    assert_eq!(listed["sessions"][0]["title"], "Read the notes.");
    assert_eq!(listed["sessions"][0]["history_len"], 1);
    let detail = home.listed(&["--sessions-v2", "session", &id], &[]);
    assert!(detail.contains("Read the notes."), "{detail}");
    assert!(detail.contains("Read it."), "{detail}");
    let traced = fs::read_to_string(&trace).expect("the trace");
    for action in [
        format!("action=Open session={id} "),
        format!("action=BeginTurn session={id} "),
        format!("action=EndTurn session={id} end=commit "),
        format!("action=Close session={id} "),
    ] {
        assert!(traced.contains(&action), "{action}{traced}");
    }
}

#[test]
fn ask_with_the_flag_after_ask_or_the_variable_saves_to_v2_and_resume_waits() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["AFTER"])),
        Reply::sse(&chat_text_events(&["VARIABLE"])),
    ]);
    let home = Home::new(&server.base_url());
    let mut saved = Vec::new();
    for (args, environment) in [
        (
            &["ask", "--sessions-v2", "--json", "Flag after ask."][..],
            &[][..],
        ),
        (
            &["ask", "--json", "From the variable."],
            &[("OH_FX_SESSIONS_V2", "true")],
        ),
    ] {
        let output = home.run(args, environment);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            text(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).expect("ask json");
        saved.push(result["session_id"].as_str().expect("id").to_owned());
    }
    let mut listed = ids(&home.listed(&["--sessions-v2", "sessions", "--json"], &[]));
    listed.sort();
    saved.sort();
    assert_eq!(listed, saved);
    let output = home.run(&["--sessions-v2", "ask", "--resume", "last", "Again."], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "oh-fx: --sessions-v2 is not available yet\n"
    );
    assert_eq!(server.requests().len(), 2);
}
