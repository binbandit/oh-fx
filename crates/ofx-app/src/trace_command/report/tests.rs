use std::fmt::Write as _;

use ofx_agent::CompactionTraceKind;
use ofx_trace::TraceContext;

use super::*;

const GENERATED_MS: i64 = 1_780_000_000_123;

fn facts() -> TraceFacts {
    TraceFacts {
        model: "model-a".to_owned(),
        fast_mode: true,
        permission_mode: PermissionMode::Auto,
        workspace_root: PathBuf::from("/work/space"),
        step_limit: 25,
        effort: ReasoningEffort::Named("high".to_owned()),
        processing: false,
        stream_active: false,
        queued: 0,
    }
}

fn snapshot() -> Snapshot {
    Snapshot {
        facts: facts(),
        generated_ms: GENERATED_MS,
        process: Process {
            pid: 42,
            open_fds: Some(7),
            memory: Some("  PID  PPID   RSS\n   42     1  1024  ".to_owned()),
        },
        trace_flag: false,
        trace_log: None,
        terminal: Terminal {
            term: Some("xterm-256color".to_owned()),
            term_program: None,
            lang: Some("en_AU.UTF-8".to_owned()),
            tmux: true,
            cmux: false,
        },
        compaction: Vec::new(),
        tool_calls: ToolCallTrace::default(),
        tail: None,
    }
}

fn compaction(
    sequence: u64,
    kind: CompactionTraceKind,
    failed: bool,
    context: TraceContext,
    detail: &str,
) -> Sequenced<CompactionEvent> {
    Sequenced {
        sequence,
        event: CompactionEvent {
            timestamp_ms: GENERATED_MS,
            context,
            failed,
            kind,
            detail: detail.to_owned(),
            truncated: false,
        },
    }
}

fn turn(turn_id: u64, step_id: u64) -> TraceContext {
    TraceContext {
        turn_id,
        step_id,
        subagent_id: 0,
    }
}

#[test]
fn a_quiet_session_reports_its_summary_state_and_runtime_context() {
    let report = snapshot().render();
    let expected = format!(
        "# oh-fx trace\n\nPrivate diagnostic report. It may include prompts, file paths, command output, and file snippets.\n\n## Summary\ngenerated: 2026-05-28T20:26:40Z\nversion: {}\nplatform: {}/{}\nbuild: {}\nmodel: model-a\nfast_mode: on\npermission_mode: auto\nworkspace: /work/space\n\n## Current State\nagent_step_limit: 25\neffort: high\nprocess: pid=42 open_fds=7\nprocess_memory:\n    PID  PPID   RSS\n     42     1  1024\nOH_FX_TRACE: off\n\n## Problems\n- no obvious errors captured in recent network, tool, compaction, MCP, or model catalog state\n\n## Context Compaction\n(none recorded)\n\n## Tool Calls\n(none recorded)\n\n## Runtime Context\nTERM: xterm-256color\nTERM_PROGRAM: (unset)\nLANG: en_AU.UTF-8\nterminal_hosts: tmux=true cmux=false\n",
        ofx_upgrade::VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH,
        if cfg!(debug_assertions) {
            "Debug"
        } else {
            "Release"
        }
    );
    assert_eq!(report, expected);
}

#[test]
fn the_trace_compaction_summary_renders_recorded_events_without_file_tracing() {
    let mut recorded = snapshot();
    recorded.compaction = vec![
        compaction(
            1,
            CompactionTraceKind::Decision,
            false,
            turn(10, 176),
            "decision=compact estimated_tokens=279466",
        ),
        compaction(
            2,
            CompactionTraceKind::RetentionExhausted,
            true,
            turn(10, 0),
            "estimated_tokens=59000",
        ),
    ];
    let report = recorded.render();
    assert!(report.contains(
        "\n## Context Compaction\nlast=2 failed=1 (always recorded; does not require OH_FX_TRACE)\n"
    ));
    assert!(report.contains(
        "[2026-05-28T20:26:40.123Z] event=decision turn_id=10 step_id=176 decision=compact estimated_tokens=279466\n"
    ));
    assert!(report.contains(
        "[2026-05-28T20:26:40.123Z] event=retention_exhausted turn_id=10 failed estimated_tokens=59000\n"
    ));
    assert!(report.contains(
        "\n## Problems\n- context compaction retention_exhausted turn_id=10 detail=estimated_tokens=59000\n\n"
    ));

    recorded.compaction = (4..=67)
        .map(|sequence| {
            compaction(
                sequence,
                CompactionTraceKind::Decision,
                false,
                turn(11, 0),
                &format!("decision=compact index={sequence}"),
            )
        })
        .collect();
    let wrapped = recorded.render();
    assert!(wrapped.contains("last=64 failed=0 overwritten_before=3 (always recorded"));
    assert!(wrapped.contains("... (40 older events omitted)\n"));
    assert!(!wrapped.contains("index=43\n"));
    assert!(wrapped.contains("index=44\n"));
}

#[test]
fn problems_show_the_newest_three_compaction_failures_with_short_details() {
    let mut recorded = snapshot();
    recorded.compaction = (1..=4)
        .map(|sequence| {
            compaction(
                sequence,
                CompactionTraceKind::OverflowRecoveryIncomplete,
                true,
                TraceContext::default(),
                &format!("{sequence}{}", "d".repeat(200)),
            )
        })
        .collect();
    let report = recorded.render();
    let problems: Vec<&str> = report
        .lines()
        .filter(|line| line.starts_with("- context compaction"))
        .collect();
    assert_eq!(problems.len(), 3);
    assert!(
        problems[0].starts_with("- context compaction overflow_recovery_incomplete detail=4ddd")
    );
    assert!(
        problems[2].starts_with("- context compaction overflow_recovery_incomplete detail=2ddd")
    );
    let detail = problems[0]
        .strip_prefix("- context compaction overflow_recovery_incomplete detail=")
        .unwrap();
    assert_eq!(detail.len(), PROBLEM_DETAIL_LIMIT + " ...".len());
    assert!(detail.ends_with(" ..."));
}

#[test]
fn an_active_turn_is_reported_as_a_problem() {
    let mut busy = snapshot();
    busy.facts.processing = true;
    busy.facts.stream_active = true;
    busy.facts.queued = 2;
    assert!(busy.render().contains(
        "\n## Problems\n- report captured an active turn; state may be partial processing=true stream_active=true queued=2\n\n"
    ));
}

#[test]
fn the_trace_tail_masks_secrets_and_keeps_the_newest_lines() {
    let mut traced = snapshot();
    traced.trace_flag = true;
    traced.trace_log = Some(PathBuf::from("/logs/trace.log"));
    let mut log = String::new();
    for index in 0..90 {
        writeln!(log, "{index} [agent] line\n").unwrap();
    }
    log.push_str(
        "1 [gateway] auth=Bearer sk-abcdefghijklmnopqrstuvwxyz0123456789 tool=exa_search\n",
    );
    log.push_str("2 [agent] ");
    log.push_str(&"x".repeat(400));
    log.push('\n');
    traced.tail = Some(Tail {
        path: PathBuf::from("/logs/trace.log"),
        bytes: log.into_bytes(),
        older_left_out: true,
    });
    let report = traced.render();
    assert!(report.contains("OH_FX_TRACE: on\ntrace_log: /logs/trace.log\n"));
    let tail = report.split("\n## Trace Tail\n").nth(1).unwrap();
    let lines: Vec<&str> = tail.lines().collect();
    assert!(lines[0].starts_with("path=/logs/trace.log last_bytes="));
    assert_eq!(lines[1], "only obvious secrets masked");
    assert_eq!(lines[2], "... (older lines truncated)");
    assert_eq!(lines.len(), 3 + TAIL_LINES);
    assert_eq!(lines[3], "12 [agent] line");
    assert!(!report.contains("sk-abcdefghijklmnopqrstuvwxyz0123456789"));
    assert!(lines[lines.len() - 2].ends_with(" tool=web_search"));
    let cut_line = lines[lines.len() - 1];
    assert_eq!(cut_line.len(), LINE_LIMIT + " ...".len());
    assert!(cut_line.ends_with("x ..."));
}

#[test]
fn trace_text_normalizes_internal_search_aliases() {
    assert_eq!(
        neutralized(
            "name=exa_search call_id=exa_search_0 fallback=parallel_search legacy=perplexity_search visible=web_search"
        ),
        "name=web_search call_id=web_search_0 fallback=web_search legacy=web_search visible=web_search"
    );
    assert!(matches!(
        neutralized("plain text"),
        Cow::Borrowed("plain text")
    ));
}

#[test]
fn timestamps_render_as_utc_milliseconds() {
    assert_eq!(
        Timestamp(GENERATED_MS).to_string(),
        "[2026-05-28T20:26:40.123Z]"
    );
    assert_eq!(Timestamp(0).to_string(), "[----------T--:--:--Z]");
    assert_eq!(file_stamp(GENERATED_MS), "2026-05-28-202640");
}

#[test]
fn a_trace_log_is_read_from_its_last_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace.log");
    assert!(read_tail(&path).is_none());
    fs::write(&path, "").unwrap();
    assert!(read_tail(&path).is_none());
    let mut contents = "a".repeat(7 * 1024);
    contents.push_str("\nlast line\n");
    fs::write(&path, &contents).unwrap();
    let tail = read_tail(&path).unwrap();
    assert!(tail.older_left_out);
    assert_eq!(tail.bytes.len(), 6 * 1024);
    assert!(tail.bytes.ends_with(b"\nlast line\n"));
}
