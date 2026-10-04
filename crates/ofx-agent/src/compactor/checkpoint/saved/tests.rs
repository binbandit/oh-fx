use super::*;

fn sample() -> Payload {
    Payload {
        entries: vec![
            Entry {
                id: "S1".to_owned(),
                text: "S1: the build passes (T2)".to_owned(),
            },
            Entry {
                id: "R3".to_owned(),
                text: "R3: keep the API stable".to_owned(),
            },
        ],
        used: vec![Used {
            kind: UsedKind::Mcp,
            name: "mcp_linear_list_issues".to_owned(),
            calls: 1,
            first_tool: 2,
            last_tool: 2,
        }],
        turns: vec![Turn {
            number: 1,
            users: vec!["Fix the build.\n\u{e9}t\u{e9}".to_owned()],
            work: "Ran the build (T1, T2).".to_owned(),
            final_reply: "Fixed.".to_owned(),
            first_tool: 1,
            last_tool: 2,
            tools: vec![
                Tool {
                    number: 1,
                    line: "shell zig build (failed, exit 1)".to_owned(),
                    why: "saw the failure".to_owned(),
                },
                Tool {
                    number: 2,
                    line: "mcp_linear_list_issues".to_owned(),
                    why: String::new(),
                },
            ],
        }],
        open: Some(OpenTurn {
            users: vec!["use the staging db".to_owned()],
            work: "Started on the tests.".to_owned(),
            text: "Looking".to_owned(),
            first_tool: 3,
            last_tool: 3,
            tools: vec![Tool {
                number: 3,
                line: "read_file src/a.zig".to_owned(),
                why: String::new(),
            }],
        }),
        turn_count: 1,
        tool_count: 3,
    }
}

#[test]
fn saved_checkpoints_use_the_upstream_marker_and_field_order() {
    let encoded = encode_checkpoint(&sample());
    let json = encoded.strip_prefix(MARKER).unwrap();
    assert_eq!(
        json,
        concat!(
            r#"{"entries":[{"id":"S1","text":"S1: the build passes (T2)"},{"id":"R3","text":"R3: keep the API stable"}],"#,
            r#""used":[{"kind":"mcp","name":"mcp_linear_list_issues","calls":1,"first_tool":2,"last_tool":2}],"#,
            r#""earlier":"","turns":[{"number":1,"users":["Fix the build.\n"#,
            "\u{e9}t\u{e9}",
            r#""],"work":"Ran the build (T1, T2).","final":"Fixed.","first_tool":1,"last_tool":2,"tools":[{"number":1,"line":"shell zig build (failed, exit 1)","why":"saw the failure"},{"number":2,"line":"mcp_linear_list_issues","why":""}]}],"#,
            r#""open":{"users":["use the staging db"],"work":"Started on the tests.","text":"Looking","first_tool":3,"last_tool":3,"tools":[{"number":3,"line":"read_file src/a.zig","why":""}]},"#,
            r#""turn_count":1,"tool_count":3,"ledger_count":0,"highest":[3,0,0,1,0],"saved":false}"#,
        )
    );
}

#[test]
fn restoring_a_saved_checkpoint_renders_what_the_model_saw() {
    let payload = sample();
    let (text, restored) = restore_checkpoint(&encode_checkpoint(&payload));
    assert_eq!(text, render(&payload));
    assert_eq!(restored, Some(payload));
    let empty = Payload::default();
    assert_eq!(
        restore_checkpoint(&encode_checkpoint(&empty)),
        (render(&empty), Some(empty))
    );
}

#[test]
fn upstream_payloads_fill_missing_fields_with_upstream_defaults() {
    let summary = format!(
        "{MARKER}{}",
        r#"{"turns":[{"number":2,"users":["hi"],"final":"hello","extra":true}],"used":[{"kind":"skill","name":"/s/SKILL.md"}],"turn_count":2,"saved":false,"future":1}"#
    );
    let (text, restored) = restore_checkpoint(&summary);
    let payload = restored.unwrap();
    assert_eq!(payload.turns[0].users, ["hi"]);
    assert_eq!(payload.turns[0].final_reply, "hello");
    assert_eq!(payload.used[0].calls, 1);
    assert_eq!(payload.used[0].kind, UsedKind::Skill);
    assert_eq!(payload.turn_count, 2);
    assert_eq!(text, render(&payload));
}

#[test]
fn checkpoints_this_port_cannot_hold_fall_back_to_their_text() {
    for json in [
        r#"{"turns":[]}"#,
        r#"{"earlier":"before","saved":false}"#,
        r#"{"ledger_count":1,"saved":false}"#,
        r#"{"turns":[{"number":1,"users":[]}],"saved":false}"#,
        r#"{"turns":[{"number":0,"users":["a"]}],"saved":false}"#,
        r#"{"open":{"users":[3]},"saved":false}"#,
        r#"{"used":[{"name":"x"}],"saved":false}"#,
        r#"{"used":[{"kind":"tool","name":"x"}],"saved":false}"#,
        r#"{"highest":[1,2],"saved":false}"#,
        r#"{"turn_count":1.5,"saved":false}"#,
        r#"{"entries":[{"id":"R1"}],"saved":false}"#,
        r#"{"turns":[{"number":1,"users":["a"],"tools":[{"line":"x"}]}],"saved":false}"#,
        r#"{"open":3,"saved":false}"#,
        r#"{"turn_count":1073741825,"saved":false}"#,
        r#"{"tool_count":18446744073709551615,"saved":false}"#,
        "[]",
        "not json",
    ] {
        assert_eq!(
            restore_checkpoint(&format!("{MARKER}{json}")),
            (json.to_owned(), None),
            "{json}"
        );
    }
}

#[test]
fn counts_up_to_the_most_any_session_numbers_are_readable() {
    let json = r#"{"turn_count":1073741824,"tool_count":1073741824,"saved":false}"#;
    let payload = restore_checkpoint(&format!("{MARKER}{json}")).1.unwrap();
    assert_eq!(payload.turn_count, 1 << 30);
    assert_eq!(payload.tool_count, 1 << 30);
}

#[test]
fn user_messages_added_while_a_turn_ran_are_saved_and_shown_after_its_first() {
    let json = r#"{"turns":[{"number":1,"users":["fix it","also the tests"],"final":"Done."}],"open":{"users":["use staging"]},"turn_count":1,"saved":false}"#;
    let (text, restored) = restore_checkpoint(&format!("{MARKER}{json}"));
    let payload = restored.unwrap();
    assert_eq!(payload.turns[0].users, ["fix it", "also the tests"]);
    assert_eq!(payload.open.as_ref().unwrap().users, ["use staging"]);
    assert!(
        text.contains("Turn 1\nUser 1:\nfix it\n\nUser 1, added while the assistant worked:\nalso the tests\n\nAssistant 1, final reply:\nDone.\n\n"),
        "{text}"
    );
    assert!(
        text.contains("Turn in progress, whose first user message follows this:\nUser, added while the assistant worked:\nuse staging\n\n"),
        "{text}"
    );
    assert_eq!(
        restore_checkpoint(&encode_checkpoint(&payload)).1,
        Some(payload)
    );
}

#[test]
fn checkpoints_without_the_marker_are_continued_as_text() {
    assert_eq!(
        restore_checkpoint("<context_handoff>older</context_handoff>"),
        (
            "This session is being continued from earlier compacted context. The summary below covers the earlier portion of the conversation.\n\n<context_handoff>older</context_handoff>\n\nRecent conversation turns are preserved verbatim.\nContinue the conversation from where it left off without asking the user to repeat context. Resume directly.".to_owned(),
            None
        )
    );
}
