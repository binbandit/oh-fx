use super::*;

const REVIEW: &str = ". Review and redact it before sharing.";

async fn trace_report(harness: &mut Harness) -> String {
    harness.clipboard.fails.store(true, Ordering::SeqCst);
    harness.command("/trace");
    let shown = harness
        .until(|event| {
            matches!(event, UiEvent::Notice { notice } if notice.topic.is_empty() && notice.body.ends_with(REVIEW))
        })
        .await;
    let Some(UiEvent::Notice { notice }) = shown.last() else {
        unreachable!()
    };
    let expected_tone = if cfg!(target_os = "macos") {
        NoticeTone::Error
    } else {
        NoticeTone::Neutral
    };
    assert_eq!(notice.tone, expected_tone);
    let saved = notice
        .body
        .strip_suffix(REVIEW)
        .and_then(|body| body.split("Trace saved at ").nth(1))
        .unwrap();
    let report = fs::read_to_string(saved).unwrap();
    fs::remove_file(saved).unwrap();
    assert_eq!(harness.clipboard.copied().last(), Some(&report));
    report
}

#[tokio::test]
async fn trace_saves_a_private_report_of_the_session_and_offers_it_to_the_clipboard() {
    let server = FakeServer::start([]);
    let mut harness = Harness::start(&server).await;
    let report = trace_report(&mut harness).await;
    assert!(report.starts_with(
        "# oh-fx trace\n\nPrivate diagnostic report. It may include prompts, file paths, command output, and file snippets.\n\n## Summary\n"
    ));
    let workspace = fs::canonicalize(harness.home.path().join("workspace")).unwrap();
    let summary = report.split("\n## Current State\n").next().unwrap();
    assert!(
        summary.ends_with(&format!(
            "\nmodel: model-a\npermission_mode: auto\nworkspace: {}\n",
            workspace.display()
        )),
        "{summary}"
    );
    assert!(report.contains("\n## Current State\nagent_step_limit: "));
    assert!(report.contains(
        "\n## Problems\n- no obvious errors captured in recent network, tool, compaction, MCP, or model catalog state\n\n## Last Shutdown\n(none recorded)\n"
    ));
    assert!(report.contains("\n## Context Compaction\n(none recorded)\n\n## Session Title\nsetting: true\ntitle: (none)\ngeneration: status=none\n\n## Network Calls\n(none recorded)\n"));
    assert!(report.contains("\n## Tool Calls\n(none recorded)\n"));
    assert!(report.contains("\n## Runtime Context\nTERM: "));
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn a_trace_taken_during_a_turn_says_the_turn_was_still_running() {
    let held = Reply::held_sse(&chat_text_events(&["streaming\n"])[..2]);
    let server = FakeServer::start([held]);
    let mut harness = Harness::start(&server).await;
    harness.submit("slow");
    harness
        .until(|event| matches!(event, UiEvent::AssistantText { .. }))
        .await;
    let report = trace_report(&mut harness).await;
    assert!(report.contains(
        "\n## Problems\n- report captured an active turn; state may be partial processing=true stream_active=true queued=0\n"
    ));
    let turn_id = harness.running_turn();
    harness.send(UiCommand::Cancel { turn_id });
    harness.until(finished(TurnOutcome::Interrupted)).await;
}

#[tokio::test]
async fn trace_reports_the_interrupted_turn_and_the_tools_it_completed() {
    let read = json!({"path": "notes.txt"}).to_string();
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events("call_1", "read_file", &read)),
        Reply::held_sse(&chat_text_events(&["partial\n"])[..2]),
    ]);
    let mut harness = Harness::start(&server).await;
    fs::write(harness.home.path().join("workspace/notes.txt"), "notes\n").unwrap();
    harness.submit("read it");
    harness
        .until(|event| matches!(event, UiEvent::AssistantText { .. }))
        .await;
    let turn_id = harness.running_turn();
    harness.send(UiCommand::Cancel { turn_id });
    harness.until(finished(TurnOutcome::Interrupted)).await;
    let report = trace_report(&mut harness).await;
    assert!(
        report.contains("\n## Problems\n- interrupted turn completed_tools=1\n"),
        "{report}"
    );
    assert!(
        report.contains("\n## Context Compaction\n(none recorded)\n\n## Interrupted Turn\nlast turn was interrupted by user\n  completed_tools (1): read_file\n\n## Session Title\n"),
        "{report}"
    );
}

#[tokio::test]
async fn trace_reports_the_session_title_the_background_generation_installed() {
    let codex = FakeServer::start([
        codex_text("Fix the renderer"),
        codex_text("Fix the renderer"),
    ]);
    let catalog = codex_catalog(false, 8);
    let mut harness = Harness::codex_saved(&codex, &catalog, &codex_settings()).await;
    chat(&mut harness, &["please fix the renderer"]).await;
    until_titled(&mut harness, "Fix the renderer").await;
    let report = trace_report(&mut harness).await;
    let section = report
        .split("\n## Session Title\n")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .unwrap();
    let session = saved_sessions(&harness.home)[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let expected = format!(
        "setting: true\ntitle: Fix the renderer\ngeneration: status=installed session={session} model={CODEX_TITLE_MODEL} elapsed="
    );
    assert!(section.starts_with(&expected), "{section}");
    assert!(section.ends_with("ms\n"), "{section}");
    assert!(!report.contains("- session title generation"), "{report}");
}

#[tokio::test]
async fn trace_reports_the_last_shutdown_the_previous_session_saved() {
    let server = FakeServer::start([]);
    let mut harness = Harness::start(&server).await;
    let diagnostics = harness.home.path().join("state/diagnostics");
    fs::create_dir_all(&diagnostics).unwrap();
    fs::write(
        diagnostics.join("last-shutdown.json"),
        r#"{"version":1,"recorded_at_ms":1700000000123,"total_ms":12,"stages":[{"name":"terminal_released","step_ms":2,"total_ms":2},{"name":"complete","step_ms":10,"total_ms":12}]}"#,
    )
    .unwrap();
    let report = trace_report(&mut harness).await;
    assert!(
        report.contains("\n## Last Shutdown\ntotal_ms=12 recorded=2023-11-14T22:13:20Z\n- terminal_released step_ms=2\n- complete step_ms=10\n\n## Context Compaction\n"),
        "{report}"
    );
    fs::write(diagnostics.join("last-shutdown.json"), "{").unwrap();
    let report = trace_report(&mut harness).await;
    assert!(
        report.contains("\n## Last Shutdown\n(last shutdown report unreadable)\n"),
        "{report}"
    );
}
