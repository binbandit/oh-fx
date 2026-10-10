use ofx_contract::{FileEvidence, FileEvidenceAction, ToolArgumentIntegrity, ToolResultStatus};
use ofx_session::{
    ArtifactCompleteness, CompactedHistory, ExecutedStep, SessionSource, ToolCallEvent,
    ToolResultEvent,
};

use super::*;

fn archive(id: &str, conversation_language: &str, turns: Vec<ArchivedTurn>) -> SessionArchive {
    SessionArchive {
        id: id.to_owned(),
        created_at_ms: 1,
        updated_at_ms: 2,
        conversation_language: conversation_language.to_owned(),
        turns,
        source: SessionSource::OhFx,
    }
}

fn read_evidence() -> FileEvidence {
    FileEvidence {
        path: "src/main.zig".to_owned(),
        new_path: None,
        tool_call_id: "call_read".to_owned(),
        tool_name: "read_file".to_owned(),
        action: FileEvidenceAction::Read,
        status: ToolResultStatus::Success,
        model_view_covers_full_file: false,
        stale: false,
    }
}

fn with_files() -> TurnExecution {
    TurnExecution {
        files: vec![read_evidence()],
        ..TurnExecution::default()
    }
}

fn rendered(archive: &SessionArchive) -> (String, String) {
    let snapshot = SessionDetailSnapshot { archive };
    (
        snapshot.render(OutputFormat::Text),
        snapshot.render(OutputFormat::Json),
    )
}

#[test]
fn core_empty_session_detail_snapshot_text_and_json_stay_stable() {
    assert_eq!(
        rendered(&archive("sess-empty", "en", Vec::new())),
        (
            "[session] sess-empty\ncreated_at_ms: 1\nupdated_at_ms: 2\nlanguage: en\nhistory_len: 0\n\n(no history yet)\n".to_owned(),
            "{\"kind\":\"session_detail\",\"id\":\"sess-empty\",\"created_at_ms\":1,\"updated_at_ms\":2,\"history_len\":0,\"conversation_language\":\"en\",\"history\":[]}\n".to_owned(),
        )
    );
}

#[test]
fn core_session_detail_snapshot_preserves_history_variant_shapes() {
    let history = archive(
        "sess-history",
        "es",
        vec![
            ArchivedTurn::Compacted(CompactedHistory {
                summary: "summary".to_owned(),
                removed_turn_count: 3,
                compaction_count: 1,
            }),
            ArchivedTurn::Replied {
                user: "hola".to_owned(),
                assistant: "que tal".to_owned(),
                execution: TurnExecution::default(),
            },
            ArchivedTurn::Replied {
                user: "npm run dev".to_owned(),
                assistant: "The historical command is no longer owned.".to_owned(),
                execution: with_files(),
            },
            ArchivedTurn::Interrupted {
                user: "inspect".to_owned(),
                assistant: Some("I inspected the entry point.".to_owned()),
                tool_call: None,
                execution: with_files(),
            },
        ],
    );
    let files = "{\"schema_version\":3,\"tool_steps\":[],\"files\":[{\"path\":\"src/main.zig\",\"new_path\":null,\"tool_call_id\":\"call_read\",\"tool_name\":\"read_file\",\"action\":\"read\",\"status\":\"success\",\"model_view_covers_full_file\":false,\"stale\":false}],\"steering\":[]}";
    assert_eq!(
        rendered(&history),
        (
            concat!(
                "[session] sess-history\ncreated_at_ms: 1\nupdated_at_ms: 2\nlanguage: es\nhistory_len: 4\n",
                "\n[turn 1]\n[compacted] removed_turns=3 compactions=1\nsummary\n",
                "\n[turn 2]\n[user]\nhola\n[assistant]\nque tal\n",
                "\n[turn 3]\n[user]\nnpm run dev\n[execution]\nfile: read success src/main.zig\n[assistant]\nThe historical command is no longer owned.\n",
                "\n[turn 4]\n[user]\ninspect\n[execution]\nfile: read success src/main.zig\n[assistant]\nI inspected the entry point.\n[interrupted]\ntool: (none)\n",
            )
            .to_owned(),
            format!(
                "{{\"kind\":\"session_detail\",\"id\":\"sess-history\",\"created_at_ms\":1,\"updated_at_ms\":2,\"history_len\":4,\"conversation_language\":\"es\",\"history\":[{{\"kind\":\"compacted_summary\",\"summary\":\"summary\",\"removed_turn_count\":3,\"compaction_count\":1}},{{\"kind\":\"assistant\",\"user\":{{\"text\":\"hola\",\"images\":[]}},\"assistant\":\"que tal\",\"execution\":{{\"schema_version\":3,\"tool_steps\":[],\"files\":[],\"steering\":[]}}}},{{\"kind\":\"assistant\",\"user\":{{\"text\":\"npm run dev\",\"images\":[]}},\"assistant\":\"The historical command is no longer owned.\",\"execution\":{files}}},{{\"kind\":\"interrupted\",\"user\":{{\"text\":\"inspect\",\"images\":[]}},\"assistant\":\"I inspected the entry point.\",\"tool_call\":null,\"completed_tool_names\":[],\"execution\":{files}}}]}}\n"
            ),
        )
    );
}

#[test]
fn core_session_detail_json_includes_assistant_execution_memory() {
    let mut result = ToolResultEvent::new(
        "fetch_1",
        "web_fetch",
        ToolResultStatus::Success,
        "result-web.txt",
        48,
        ArtifactCompleteness::Complete,
    );
    result.preview = Some("<artifact_handle>artifact-file.pdf</artifact_handle>".to_owned());
    let call = ToolCallEvent::new(
        "fetch_1",
        "web_fetch",
        "{\"url\":\"https://example.com/file.pdf\",\"prompt\":\"[REDACTED]\"}",
        ToolArgumentIntegrity::Valid,
    );
    let execution = TurnExecution {
        steps: vec![ExecutedStep {
            assistant: Some("Fetching artifact.".to_owned()),
            calls: vec![call],
            results: vec![result],
        }],
        ..TurnExecution::default()
    };
    let exec = archive(
        "sess-exec",
        "en",
        vec![ArchivedTurn::Replied {
            user: "fetch pdf".to_owned(),
            assistant: "artifact saved".to_owned(),
            execution: execution.clone(),
        }],
    );
    let (text, json) = rendered(&exec);
    assert_eq!(
        text,
        concat!(
            "[session] sess-exec\ncreated_at_ms: 1\nupdated_at_ms: 2\nlanguage: en\nhistory_len: 1\n",
            "\n[turn 1]\n[user]\nfetch pdf\n[execution]\nassistant:\nFetching artifact.\n",
            "tool_call: fetch_1 web_fetch\narguments:\n{\"url\":\"https://example.com/file.pdf\",\"prompt\":\"[REDACTED]\"}\n",
            "tool_result: fetch_1 web_fetch success\noutput:\n<artifact_handle>artifact-file.pdf</artifact_handle>\n",
            "[assistant]\nartifact saved\n",
        )
    );
    assert_eq!(
        json,
        format!(
            "{{\"kind\":\"session_detail\",\"id\":\"sess-exec\",\"created_at_ms\":1,\"updated_at_ms\":2,\"history_len\":1,\"conversation_language\":\"en\",\"history\":[{{\"kind\":\"assistant\",\"user\":{{\"text\":\"fetch pdf\",\"images\":[]}},\"assistant\":\"artifact saved\",\"execution\":{}}}]}}\n",
            execution.presentation_json()
        )
    );
    assert!(json.contains("\"execution\":{\"schema_version\":3"));
    assert!(!json.contains("turn_summary"));
}

#[test]
fn interruptions_name_the_call_they_cut_short_and_empty_text_says_so() {
    let call = ToolCallEvent::new("call-9", "shell", "", ToolArgumentIntegrity::Valid);
    let cut = archive(
        "sess-cut",
        "en",
        vec![
            ArchivedTurn::Interrupted {
                user: String::new(),
                assistant: None,
                tool_call: Some(call),
                execution: TurnExecution::default(),
            },
            ArchivedTurn::Compacted(CompactedHistory {
                summary: "fx-compactor-v1\nnot json".to_owned(),
                removed_turn_count: 1,
                compaction_count: 1,
            }),
            ArchivedTurn::Replied {
                user: "multi\n".to_owned(),
                assistant: String::new(),
                execution: TurnExecution::default(),
            },
        ],
    );
    assert_eq!(
        rendered(&cut),
        (
            concat!(
                "[session] sess-cut\ncreated_at_ms: 1\nupdated_at_ms: 2\nlanguage: en\nhistory_len: 3\n",
                "\n[turn 1]\n[user]\n(empty)\n[interrupted]\ntool_call_id: call-9\ntool_name: shell\n",
                "\n[turn 2]\n[compacted] removed_turns=1 compactions=1\nnot json\n",
                "\n[turn 3]\n[user]\nmulti\n[assistant]\n(empty)\n",
            )
            .to_owned(),
            concat!(
                "{\"kind\":\"session_detail\",\"id\":\"sess-cut\",\"created_at_ms\":1,\"updated_at_ms\":2,\"history_len\":3,\"conversation_language\":\"en\",\"history\":[",
                "{\"kind\":\"interrupted\",\"user\":{\"text\":\"\",\"images\":[]},\"assistant\":null,\"tool_call\":{\"id\":\"call-9\",\"name\":\"shell\",\"arguments_json\":\"\"},\"completed_tool_names\":[]},",
                "{\"kind\":\"compacted_summary\",\"summary\":\"fx-compactor-v1\\nnot json\",\"removed_turn_count\":1,\"compaction_count\":1},",
                "{\"kind\":\"assistant\",\"user\":{\"text\":\"multi\\n\",\"images\":[]},\"assistant\":\"\",\"execution\":{\"schema_version\":3,\"tool_steps\":[],\"files\":[],\"steering\":[]}}]}\n",
            )
            .to_owned(),
        )
    );
}

#[test]
fn a_session_fx_saved_carries_its_marker_after_upstreams_fields() {
    let mut saved_by_fx = archive("fx-one", "en", Vec::new());
    saved_by_fx.source = SessionSource::Fx;
    assert_eq!(
        rendered(&saved_by_fx),
        (
            "[session] fx-one\ncreated_at_ms: 1\nupdated_at_ms: 2\nlanguage: en\nhistory_len: 0\nsource: fx\n\n(no history yet)\n".to_owned(),
            "{\"kind\":\"session_detail\",\"id\":\"fx-one\",\"created_at_ms\":1,\"updated_at_ms\":2,\"history_len\":0,\"conversation_language\":\"en\",\"history\":[],\"source\":\"fx\"}\n".to_owned(),
        )
    );
}
