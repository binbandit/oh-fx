use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;

pub(crate) const GENERATION: &str = "01010101010101010101010101010101";
const AUTHORITY_ID: &str = "03030303030303030303030303030303";
const USAGE: &str = "{\"billing\":\"complete\",\"api_duration_complete\":true,\"wall_duration_complete\":true,\"code_complete\":true,\"next_sequence\":1,\"settled_through_sequence\":0,\"api_duration_ms\":0,\"wall_duration_ms\":0,\"total_cost\":0,\"input_tokens\":0,\"output_tokens\":0,\"cache_read_tokens\":0,\"cache_write_tokens\":0,\"billable_web_search_calls\":0,\"lines_added\":0,\"lines_removed\":0,\"models\":[],\"pending\":[]}";

pub(crate) struct LegacyLog {
    id: String,
    workspace: String,
    frames: Vec<(String, String)>,
    committed: Option<usize>,
    tail: String,
    display_title: Option<String>,
}

impl LegacyLog {
    pub(crate) fn started(id: &str, workspace: &str) -> Self {
        Self::with_preferences(
            id,
            workspace,
            "{\"model\":\"openai/gpt-5\",\"effort\":\"auto\",\"fast_mode\":false}",
        )
    }

    pub(crate) fn with_preferences(id: &str, workspace: &str, preferences: &str) -> Self {
        let started = format!(
            "{{\"id\":\"{id}\",\"created_at_ms\":10,\"origin_workspace_root\":\"{workspace}\",\"workspace_root\":\"{workspace}\",\"conversation_language\":\"en\",\"preferences\":{preferences},\"usage\":{USAGE}}}"
        );
        Self {
            id: id.to_owned(),
            workspace: workspace.to_owned(),
            frames: vec![("session_started".to_owned(), started)],
            committed: None,
            tail: String::new(),
            display_title: None,
        }
    }

    #[must_use]
    pub(crate) fn frame(mut self, kind: &str, payload: &str) -> Self {
        self.frames.push((kind.to_owned(), payload.to_owned()));
        self
    }

    #[must_use]
    pub(crate) fn turn(self, turn: &str) -> Self {
        self.frame(
            "history_turn_committed",
            &format!(
                "{{\"conversation_language\":\"en\",\"total_input_tokens\":0,\"total_output_tokens\":0,\"turn\":{turn}}}"
            ),
        )
        .frame("usage_checkpointed", &format!("{{\"usage\":{USAGE}}}"))
    }

    #[must_use]
    pub(crate) fn committed_through(mut self, frames: usize) -> Self {
        self.committed = Some(frames);
        self
    }

    #[must_use]
    pub(crate) fn tail(mut self, tail: &str) -> Self {
        tail.clone_into(&mut self.tail);
        self
    }

    #[must_use]
    pub(crate) fn titled(mut self, title: &str) -> Self {
        self.display_title = Some(title.to_owned());
        self
    }

    pub(crate) fn events(&self) -> String {
        let mut log = String::new();
        for (index, (kind, payload)) in self.frames.iter().enumerate() {
            let seq = index + 1;
            let _ = writeln!(
                log,
                "{{\"schema_version\":1,\"log_generation\":\"{GENERATION}\",\"seq\":{seq},\"event_id\":\"{}\",\"timestamp_ms\":{},\"kind\":\"{kind}\",\"payload\":{payload}}}",
                event_id(seq),
                seq * 10
            );
        }
        log
    }

    pub(crate) fn watermark(&self) -> String {
        let committed = self.committed.unwrap_or(self.frames.len());
        let bytes: usize = self
            .events()
            .split_inclusive('\n')
            .take(committed)
            .map(str::len)
            .sum();
        format!(
            "{{\"schema_version\":1,\"session_id\":\"{}\",\"log_generation\":\"{GENERATION}\",\"through_seq\":{committed},\"through_event_id\":\"{}\",\"through_event_log_bytes\":{bytes}}}\n",
            self.id,
            event_id(committed)
        )
    }

    fn manifest(&self) -> String {
        format!(
            "{{\"schema_version\":3,\"storage_format\":\"event_log_v1\",\"id\":\"{id}\",\"authority_id\":\"{AUTHORITY_ID}\",\"log_generation\":\"{GENERATION}\",\"created_at_ms\":10,\"updated_at_ms\":10,\"origin_workspace_root\":\"{workspace}\",\"workspace_root\":\"{workspace}\",\"conversation_language\":\"en\",\"history_len\":0,\"total_input_tokens\":0,\"total_output_tokens\":0,\"last_event_seq\":1,\"event_log_bytes\":1,\"event_log_stat_fingerprint\":\"{fingerprint}\",\"generation_base_seq\":1,\"generation_base_bytes\":1,\"checkpoint_seq\":null,\"checkpoint_sha256\":null,\"preferences\":{{\"model\":\"openai/gpt-5\",\"effort\":\"auto\",\"fast_mode\":false,\"provider\":\"gateway\"}}}}",
            id = self.id,
            workspace = self.workspace,
            fingerprint = "00".repeat(32),
        )
    }

    pub(crate) fn write(&self, sessions: &Path) -> PathBuf {
        let dir = sessions.join(&self.id);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let authority = format!(
            "{{\"schema_version\":1,\"session_id\":\"{}\",\"authority_id\":\"{AUTHORITY_ID}\",\"storage_format\":\"event_log_v1\",\"source\":\"native_create\"}}\n",
            self.id
        );
        let mut files = vec![
            ("authority.json".to_owned(), authority),
            ("events.jsonl".to_owned(), self.events() + &self.tail),
            (format!("commit.{GENERATION}.json"), self.watermark()),
            ("session.json".to_owned(), self.manifest()),
            ("commit.lock".to_owned(), String::new()),
        ];
        if let Some(title) = &self.display_title {
            files.push((
                "display.json".to_owned(),
                format!(
                    "{{\"schema_version\":1,\"title\":\"{title}\",\"preview\":null,\"origin_workspace_root\":\"{}\"}}",
                    self.workspace
                ),
            ));
        }
        for (name, bytes) in files {
            let path = dir.join(name);
            fs::write(&path, bytes).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        dir
    }
}

pub(crate) fn event_id(seq: usize) -> String {
    format!("{:032x}", seq + 0x100)
}

pub(crate) fn reply(prompt: &str, answer: &str) -> String {
    format!(
        "{{\"kind\":\"assistant\",\"user\":{{\"text\":\"{prompt}\",\"images\":[]}},\"assistant\":\"{answer}\",\"execution\":{{\"schema_version\":3,\"tool_steps\":[],\"files\":[]}}}}"
    )
}

pub(crate) fn command_result(call_id: &str, output: &str, handle: &str) -> String {
    format!(
        "{{\"tool_call_id\":\"{call_id}\",\"tool_name\":\"run_command\",\"status\":\"success\",\"output\":\"{output}\",\"output_handle\":{handle},\"preview\":null,\"output_bytes\":{},\"stored_output_bytes\":{},\"truncated\":false,\"provider_native\":false,\"created_at_ms\":15,\"permission_feedback\":[],\"committed_file_presentation\":null,\"command_output_replay\":null,\"command_process_presentation\":{{\"kind\":\"exit_code\",\"value\":0}}}}",
        output.len(),
        output.len()
    )
}

pub(crate) fn command_turn(prompt: &str, call_id: &str, result: &str, answer: &str) -> String {
    format!(
        "{{\"kind\":\"assistant\",\"user\":{{\"text\":\"{prompt}\",\"images\":[]}},\"assistant\":\"{answer}\",\"execution\":{{\"schema_version\":3,\"tool_steps\":[{{\"assistant\":\"Checking.\",\"tool_calls\":[{{\"id\":\"{call_id}\",\"name\":\"run_command\",\"arguments_json\":\"{{\\\"command\\\":\\\"ls\\\"}}\",\"provider_result\":null}}],\"tool_results\":[{result}]}}],\"files\":[{{\"path\":\"src/main.rs\",\"new_path\":null,\"tool_call_id\":\"{call_id}\",\"tool_name\":\"read_file\",\"action\":\"read\",\"status\":\"success\",\"model_view_covers_full_file\":true,\"stale\":false}},{{\"path\":\"\",\"new_path\":null,\"tool_call_id\":\"{call_id}\",\"tool_name\":\"read_file\",\"action\":\"read\",\"status\":\"success\",\"model_view_covers_full_file\":false,\"stale\":false}}]}}}}"
    )
}

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }

    fn dir(&self, log: &LegacyLog) -> PrivateDir {
        log.write(self.root.path());
        PrivateDir::open_existing(self.root.path())
            .unwrap()
            .unwrap()
            .open_child(&log.id)
            .unwrap()
            .unwrap()
    }

    fn summary(&self, log: &LegacyLog) -> Result<Option<SessionSummary>, SessionError> {
        summarize_schema_v3(&self.dir(log), &log.id)
    }
}

#[test]
fn a_conversation_fx_saved_before_0_0_8_lists_from_its_committed_log() {
    let fixture = Fixture::new();
    let log = LegacyLog::started("legacy-one", "/work")
        .turn(&reply("first prompt here", "first answer"))
        .turn(&command_turn(
            "list files",
            "call_1",
            &command_result("call_1", "a b", "null"),
            "done",
        ))
        .titled("Saved display title");
    let summary = fixture.summary(&log).unwrap().unwrap();
    assert_eq!(summary.id, "legacy-one");
    assert_eq!(summary.workspace_root, "/work");
    assert_eq!(summary.origin_workspace_root, "/work");
    assert_eq!(summary.title.as_deref(), Some("Saved display title"));
    assert_eq!(summary.created_at_ms, 10);
    assert_eq!(summary.updated_at_ms, 50);
    assert_eq!(summary.conversation_language, "en");
    assert_eq!(summary.history_len, 2);
    assert!(!summary.has_checkpoint);

    let untitled = LegacyLog::started("legacy-two", "/work").turn(&reply("one", "two"));
    assert_eq!(fixture.summary(&untitled).unwrap().unwrap().title, None);
}

#[test]
fn only_the_prefix_the_watermark_commits_is_read() {
    let fixture = Fixture::new();
    let log = LegacyLog::started("legacy-prefix", "/work")
        .turn(&reply("kept", "answer"))
        .turn(&reply("never acknowledged", "lost"))
        .committed_through(3)
        .tail("{\"torn");
    let summary = fixture.summary(&log).unwrap().unwrap();
    assert_eq!(summary.history_len, 1);
    assert_eq!(summary.updated_at_ms, 30);
}

#[test]
fn a_watermark_that_does_not_match_the_log_makes_the_session_unreadable() {
    let fixture = Fixture::new();
    let base = || LegacyLog::started("legacy-mark", "/work").turn(&reply("one", "two"));
    let cases: [(&str, fn(String) -> String); 6] = [
        ("session", |mark| {
            mark.replace("legacy-mark", "other-session")
        }),
        ("generation", |mark| {
            mark.replace(GENERATION, &"02".repeat(16))
        }),
        ("event", |mark| mark.replace(&event_id(3), &event_id(2))),
        ("seq", |mark| {
            mark.replace("\"through_seq\":3", "\"through_seq\":2")
        }),
        ("bytes", |mark| {
            mark.replace(
                "\"through_event_log_bytes\":",
                "\"through_event_log_bytes\":1",
            )
        }),
        ("version", |mark| {
            mark.replace("{\"schema_version\":1", "{\"schema_version\":2")
        }),
    ];
    for (case, edit) in cases {
        let log = base();
        let dir = fixture.dir(&log);
        let path = fixture
            .root
            .path()
            .join("legacy-mark")
            .join(format!("commit.{GENERATION}.json"));
        fs::write(&path, edit(log.watermark())).unwrap();
        assert!(summarize_schema_v3(&dir, "legacy-mark").is_err(), "{case}");
    }
    let log = base();
    let dir = fixture.dir(&log);
    fs::remove_file(
        fixture
            .root
            .path()
            .join("legacy-mark")
            .join(format!("commit.{GENERATION}.json")),
    )
    .unwrap();
    assert!(summarize_schema_v3(&dir, "legacy-mark").is_err());
}

#[test]
fn a_log_whose_frames_break_sequence_or_shape_is_unreadable() {
    let fixture = Fixture::new();
    let cases = [
        LegacyLog::started("legacy-frames", "/work")
            .frame("workspace_rebound", "{\"previous_workspace_root\":\"/elsewhere\",\"workspace_root\":\"/new\"}"),
        LegacyLog::started("legacy-frames", "/work")
            .frame("session_started", "{}"),
        LegacyLog::started("legacy-frames", "/work").frame("preferences_changed", "{}"),
        LegacyLog::started("legacy-frames", "/work").frame("unknown_kind", "{}"),
        LegacyLog::started("legacy-frames", "/work")
            .frame("history_turn_committed", "{\"conversation_language\":\"en\",\"total_input_tokens\":0,\"total_output_tokens\":0,\"turn\":{\"kind\":\"assistant\"}}"),
    ];
    for log in cases {
        assert!(fixture.summary(&log).is_err(), "{}", log.events());
    }
    let log = LegacyLog::started("legacy-frames", "/work").turn(&reply("one", "two"));
    let dir = fixture.dir(&log);
    let renumbered = log.events().replace("\"seq\":2", "\"seq\":4");
    fs::write(
        fixture
            .root
            .path()
            .join("legacy-frames")
            .join("events.jsonl"),
        renumbered,
    )
    .unwrap();
    assert!(summarize_schema_v3(&dir, "legacy-frames").is_err());
}

#[test]
fn logs_holding_what_oh_fx_cannot_convert_yet_stay_unreadable() {
    let fixture = Fixture::new();
    let image_turn = reply("look", "seen").replace(
        "\"images\":[]",
        "\"images\":[{\"id\":1,\"path\":\"/tmp/a.png\",\"media_type\":\"image/png\",\"snapshot_path\":null,\"snapshot_sha256\":null}]",
    );
    let replay_turn = command_turn(
        "list",
        "call_1",
        &command_result("call_1", "a", "null").replace(
            "\"command_output_replay\":null",
            "\"command_output_replay\":{\"handle\":\"fx-command-replay-1.bin\",\"framed_bytes\":8}",
        ),
        "done",
    );
    let compacted = "{\"kind\":\"compacted_summary\",\"summary\":\"earlier\",\"removed_turn_count\":1,\"compaction_count\":1}";
    for log in [
        LegacyLog::started("legacy-later", "/work").turn(&image_turn),
        LegacyLog::started("legacy-later", "/work").turn(&replay_turn),
        LegacyLog::started("legacy-later", "/work").turn(compacted),
        LegacyLog::started("legacy-later", "/work").frame("recovery_checkpoint_cleared", "{}"),
        LegacyLog::started("legacy-later", "/work").frame(
            "state_replacement_started",
            &format!(
                "{{\"replacement_id\":\"{GENERATION}\",\"reason\":\"compaction\",\"encoded_bytes\":1,\"sha256\":\"{}\",\"chunk_count\":1}}",
                "00".repeat(32)
            ),
        ),
    ] {
        assert!(fixture.summary(&log).is_err(), "{}", log.events());
    }
}

#[test]
fn a_subagent_child_is_not_listed() {
    let fixture = Fixture::new();
    let log = LegacyLog::started("legacy-child", "/work").turn(&reply("one", "two"));
    let log = LegacyLog {
        frames: log
            .frames
            .into_iter()
            .map(|(kind, payload)| {
                (
                    kind,
                    payload.replace(
                        &format!(",\"usage\":{USAGE}}}"),
                        &format!(",\"usage\":{USAGE},\"subagent_child\":true}}"),
                    ),
                )
            })
            .collect(),
        ..log
    };
    assert_eq!(fixture.summary(&log).unwrap(), None);
}

#[test]
fn preference_and_workspace_changes_replay_in_order() {
    let fixture = Fixture::new();
    let log = LegacyLog::with_preferences(
        "legacy-moved",
        "/work",
        "{\"connection_id\":\"vercel\",\"model_id\":\"openai/gpt-5\",\"effort\":\"high\",\"fast_mode\":true}",
    )
    .turn(&reply("one", "two"))
    .frame(
        "preferences_changed",
        "{\"model\":\"openai/gpt-5.1\",\"fast_mode\":false}",
    )
    .frame(
        "workspace_rebound",
        "{\"previous_workspace_root\":\"/work\",\"workspace_root\":\"/moved\"}",
    );
    let session = load_schema_v3(&fixture.dir(&log), "legacy-moved").unwrap();
    assert_eq!(session.workspace_root, "/moved");
    assert_eq!(session.origin_workspace_root, "/work");
    assert_eq!(session.preferences.provider.id().label(), "gateway");
    assert_eq!(session.preferences.model, "openai/gpt-5.1");
    assert_eq!(session.preferences.effort.label(), "high");
    assert!(!session.preferences.fast_mode);
    assert_eq!(session.updated_at_ms, 50);

    let foreign = LegacyLog::with_preferences(
        "legacy-foreign",
        "/work",
        "{\"connection_id\":\"other\",\"model_id\":\"openai/gpt-5\",\"effort\":\"high\",\"fast_mode\":true}",
    );
    assert!(fixture.summary(&foreign).is_err());
}
