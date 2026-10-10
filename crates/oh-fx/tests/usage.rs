use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const HOUR_MS: i64 = 60 * 60 * 1000;
const DAY_MS: i64 = 24 * HOUR_MS;

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = fs::canonicalize(directory.path()).expect("canonicalize the home");
        Self {
            _directory: directory,
            root,
        }
    }

    fn data(&self) -> PathBuf {
        self.root.join("data/oh-fx")
    }

    fn write_ledger(&self, records: &[String]) {
        fs::create_dir_all(self.data()).expect("create the data directory");
        fs::set_permissions(self.data(), fs::Permissions::from_mode(0o700))
            .expect("make it private");
        let ledger = self.data().join("usage.jsonl");
        fs::write(&ledger, records.concat()).expect("write the ledger");
        fs::set_permissions(&ledger, fs::Permissions::from_mode(0o600)).expect("make it private");
    }

    fn run(&self, args: &[&str], data_home: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .arg("usage")
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", &self.root)
            .env("OH_FX_AUTO_UPGRADE", "0")
            .stdin(Stdio::null());
        if data_home {
            command.env("XDG_DATA_HOME", self.root.join("data"));
        }
        command.output().expect("run oh-fx usage")
    }
}

fn now_ms() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after the epoch");
    i64::try_from(elapsed.as_millis()).expect("milliseconds fit")
}

fn coverage(started_at_ms: i64) -> String {
    format!("{{\"schema_version\":1,\"kind\":\"coverage\",\"started_at_ms\":{started_at_ms}}}\n")
}

fn generation(
    id: &str,
    created_at_ms: i64,
    model: &str,
    tokens: (u64, u64, u64),
    reasoning: &str,
) -> String {
    let (input, output, cached) = tokens;
    format!(
        "{{\"schema_version\":1,\"kind\":\"generation\",\"fact\":{{\"id\":\"{id}\",\"created_at_ms\":{created_at_ms},\"model\":\"{model}\",\"input_tokens\":{input},\"output_tokens\":{output},\"cache_read_tokens\":{cached},\"cache_write_tokens\":0,\"reasoning_tokens\":{reasoning},\"billable_web_search_calls\":0,\"total_cost\":0}}}}\n"
    )
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("UTF-8 output")
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("one JSON line")
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

#[test]
fn an_empty_profile_has_not_started_tracking_and_stays_empty() {
    let home = Home::new();
    for (args, heading) in [
        (&[][..], "Usage (30 days)"),
        (&["--period", "7d"], "Usage (7 days)"),
        (&["--period", "24h"], "Usage (24 hours)"),
    ] {
        let output = home.run(args, false);
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert_eq!(text(&output.stderr), "", "{args:?}");
        assert_eq!(
            text(&output.stdout),
            format!("{heading}\nTracking has not started.\n"),
            "{args:?}"
        );
    }
    let before = now_ms();
    let output = home.run(&["--json", "--period", "7d"], false);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.ends_with(b"}\n"));
    let report = json(&output);
    let snapshot_time_ms = report["snapshot_time_ms"]
        .as_i64()
        .expect("a snapshot time");
    assert!(snapshot_time_ms >= before);
    assert_eq!(
        report["window_start_ms"].as_i64(),
        Some(snapshot_time_ms - 7 * DAY_MS)
    );
    assert_eq!(report["kind"], "usage");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["period"], "7d");
    assert_eq!(
        report["coverage"],
        serde_json::json!({"status": "not_started", "started_at_ms": null, "full_window": false})
    );
    assert_eq!(report["completeness"], "complete");
    assert_eq!(report["totals"], Value::Null);
    assert_eq!(report["models"], serde_json::json!([]));
    assert!(!exists(&home.root.join(".local/share/oh-fx")));
}

#[test]
fn the_report_sums_the_ledger_fx_writes_per_window() {
    let home = Home::new();
    let now = now_ms();
    home.write_ledger(&[
        coverage(now - 40 * DAY_MS),
        generation(
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            now - HOUR_MS,
            "portkey/gpt-4o",
            (1200, 300, 200),
            "null",
        ),
        generation(
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAW",
            now - 2 * HOUR_MS,
            "codex/gpt-5",
            (100, 50, 0),
            "10",
        ),
        generation(
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAX",
            now - 2 * DAY_MS,
            "codex/gpt-5",
            (1000, 100, 0),
            "20",
        ),
    ]);

    let output = home.run(&[], true);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "Usage (30 days)\nTotal tokens  2750\nInput         2300\nOutput        450\nCache         200 read · 0 write\nRequests      3\nSpend         $0.0000\n\nBy model\n- portkey/gpt-4o  1500 tokens  $0.0000\n- codex/gpt-5  1250 tokens  $0.0000\n"
    );

    let output = home.run(&["--period", "24h", "--json"], true);
    assert_eq!(output.status.code(), Some(0));
    let report = json(&output);
    assert_eq!(report["period"], "24h");
    assert_eq!(report["coverage"]["status"], "full");
    assert_eq!(
        report["coverage"]["started_at_ms"].as_i64(),
        Some(now - 40 * DAY_MS)
    );
    assert_eq!(report["coverage"]["full_window"], true);
    assert_eq!(report["completeness"], "complete");
    assert_eq!(
        report["totals"],
        serde_json::json!({
            "total_tokens": 1650,
            "input_tokens": 1300,
            "output_tokens": 350,
            "cache_read_tokens": 200,
            "cache_write_tokens": 0,
            "reasoning_tokens": null,
            "request_count": 2,
            "spend": 0,
        })
    );
    let models: Vec<&str> = report["models"]
        .as_array()
        .expect("model rows")
        .iter()
        .map(|row| row["model"].as_str().expect("a model name"))
        .collect();
    assert_eq!(models, ["portkey/gpt-4o", "codex/gpt-5"]);
    assert_eq!(report["models"][1]["totals"]["reasoning_tokens"], 10);
}

#[test]
fn a_recent_tracking_start_reports_a_partial_window() {
    let home = Home::new();
    let started_at_ms = now_ms() - HOUR_MS;
    home.write_ledger(&[coverage(started_at_ms)]);
    let output = home.run(&["--period", "7d"], true);
    assert_eq!(output.status.code(), Some(0));
    let stdout = text(&output.stdout);
    assert!(
        stdout.starts_with("Usage (7 days)\nTracking since "),
        "{stdout}"
    );
    assert!(
        stdout.contains(" (partial window).\nTotal tokens  0\n"),
        "{stdout}"
    );
    assert!(
        stdout.ends_with("Requests      0\nSpend         $0.0000\n"),
        "{stdout}"
    );
}

#[test]
fn unsafe_or_unreadable_usage_state_fails_with_upstream_messages() {
    let home = Home::new();
    home.write_ledger(&[coverage(0)]);
    fs::set_permissions(home.data(), fs::Permissions::from_mode(0o755)).expect("loosen the mode");
    let output = home.run(&[], true);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "oh-fx usage: local usage storage is unsafe\n"
    );
    let output = home.run(&["--json"], true);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stdout),
        "{\"kind\":\"usage\",\"error\":\"local usage storage is unsafe\",\"code\":\"PrivateStatePermissionsUnsupported\"}\n"
    );

    let home = Home::new();
    home.write_ledger(&[
        coverage(0),
        "{\"schema_version\":1,\"kind\":\"unknown\"}\n".to_owned(),
    ]);
    let output = home.run(&["--period", "7d"], true);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "oh-fx usage: local usage data is unavailable\n"
    );
    let output = home.run(&["--json"], true);
    assert_eq!(
        text(&output.stdout),
        "{\"kind\":\"usage\",\"error\":\"local usage data is unavailable\",\"code\":\"InvalidUsageStore\"}\n"
    );
}
