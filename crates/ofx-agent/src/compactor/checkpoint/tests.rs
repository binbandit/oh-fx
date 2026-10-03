use super::*;

fn tool(number: usize, line: &str, why: &str) -> Tool {
    Tool {
        number,
        line: line.to_owned(),
        why: why.to_owned(),
    }
}

fn entry(id: &str, text: &str) -> Entry {
    Entry {
        id: id.to_owned(),
        text: text.to_owned(),
    }
}

fn turn(number: usize, user: &str, final_reply: &str) -> Turn {
    Turn {
        number,
        users: vec![user.to_owned()],
        work: String::new(),
        final_reply: final_reply.to_owned(),
        first_tool: 0,
        last_tool: 0,
        tools: Vec::new(),
    }
}

fn sample_payload() -> Payload {
    Payload {
        turns: vec![
            Turn {
                work: "Found a missing semicolon (T4), fixed it, and ran the tests (T5)."
                    .to_owned(),
                first_tool: 4,
                last_tool: 5,
                tools: vec![
                    tool(
                        4,
                        "shell zig build (failed, exit 1, 3 lines)",
                        "built to see the failure; a missing semicolon at src/a.zig:4",
                    ),
                    tool(5, "shell zig build test (exit 0, 12 lines)", ""),
                ],
                ..turn(
                    2,
                    "Fix the build.\nIt fails on main.",
                    "Fixed. The build and all 12 tests pass.",
                )
            },
            turn(3, "thanks", "You're welcome."),
        ],
        entries: vec![
            entry("S1", "S1: the build and tests pass (T5)"),
            entry("R1", "R1 [M2]: \"also check the tests\""),
            entry("F1", "F1 (T4): src/a.zig:4 was missing a semicolon"),
            entry("O1", "O1: nothing open"),
        ],
        turn_count: 3,
        tool_count: 5,
        ..Payload::default()
    }
}

#[test]
fn payloads_render_each_turn_in_order_then_the_entries() {
    let text = render(&sample_payload());
    let expected = "Turn 2
User 2:
Fix the build.
It fails on main.

Assistant 2, in between:
Found a missing semicolon (T4), fixed it, and ran the tests (T5).

Tools:
  T4 shell zig build (failed, exit 1, 3 lines): built to see the failure; a missing semicolon at src/a.zig:4
  T5 shell zig build test (exit 0, 12 lines)

Assistant 2, final reply:
Fixed. The build and all 12 tests pass.

Turn 3
User 3:
thanks

Assistant 3, final reply:
You're welcome.

Rules of the session:
R1 [M2]: \"also check the tests\"

Facts of the session:
F1 (T4): src/a.zig:4 was missing a semicolon

Status and open:
S1: the build and tests pass (T5)
O1: nothing open

</compacted_conversation>
";
    assert!(text.ends_with(expected), "{text}");
    assert!(text.starts_with(
        "<compacted_conversation>\nThis is the earlier part of this conversation, compacted. "
    ));
    assert!(!text.contains("read_tool_result"));
    assert!(!text.contains("Saved word for word"));
}

#[test]
fn the_turn_in_progress_shows_its_summary_and_tools() {
    let text = render(&Payload {
        open: Some(OpenTurn {
            users: Vec::new(),
            work: "Ran the migration dry run (T1).".to_owned(),
            text: "exact text".to_owned(),
            first_tool: 1,
            last_tool: 1,
            tools: vec![tool(1, "shell migrate --dry-run (18 bytes)", "dry run")],
        }),
        tool_count: 1,
        ..Payload::default()
    });
    assert!(text.contains("Turn in progress, whose first user message follows this:\nAssistant, in between so far:\nRan the migration dry run (T1).\n\nIts tools so far:\n  T1 shell migrate --dry-run (18 bytes): dry run\n\n"));
    assert!(!text.contains("exact text"));
}

#[test]
fn an_entry_a_later_one_replaces_is_shown_replaced_and_check_marks_are_explained() {
    let marked = render(&Payload {
        entries: vec![
            entry("S1", "S1 (T1): tests fail"),
            entry("D1", "D1 (M1): cache the index"),
            entry("S2", "S2 (T2): tests pass; replaces S1 and D1."),
            entry(
                "F1",
                "F1 (T2): 142 tests [check: not in the saved record: 142]",
            ),
        ],
        turn_count: 1,
        tool_count: 2,
        ..Payload::default()
    });
    assert!(marked.contains("S1 (T1): tests fail (replaced by S2)\n"));
    assert!(marked.contains("D1 (M1): cache the index (replaced by S2)\n"));
    assert!(marked.contains("S2 (T2): tests pass; replaces S1 and D1.\n"));
    assert!(marked.contains("A note or entry marked [check: ...]"));

    let clean = render(&Payload {
        entries: vec![entry("F1", "F1 (T2): 141 tests")],
        turn_count: 1,
        tool_count: 2,
        ..Payload::default()
    });
    assert!(!clean.contains("[check:"));
    assert!(!clean.contains("replaced by"));
}

#[test]
fn the_shape_check_passes_what_compaction_builds_and_names_what_breaks_it() {
    let first_tools = vec![tool(1, "shell make (2 bytes)", "")];
    let earlier = Payload {
        turns: vec![Turn {
            first_tool: 1,
            last_tool: 1,
            tools: first_tools.clone(),
            ..turn(1, "build it", "Built.")
        }],
        entries: vec![entry("F1", "F1 (T1): make works")],
        turn_count: 1,
        tool_count: 1,
        ..Payload::default()
    };
    let new_tools = vec![
        tool(2, "shell make test (3 lines)", ""),
        tool(3, "read_file a.zig (9 lines)", ""),
    ];
    let new_turn = Turn {
        first_tool: 2,
        last_tool: 3,
        tools: new_tools.clone(),
        ..turn(2, "test it", "Tested.")
    };
    let good = Payload {
        turns: vec![earlier.turns[0].clone(), new_turn.clone()],
        entries: vec![
            earlier.entries[0].clone(),
            entry("S1", "S1 (T2): tests pass"),
        ],
        turn_count: 2,
        tool_count: 3,
        ..Payload::default()
    };
    assert_eq!(shape_problem(&earlier, &good), None);

    let mut bad = good.clone();
    bad.turns[0].users = vec!["build it again".to_owned()];
    assert_eq!(
        shape_problem(&earlier, &bad),
        Some("an earlier turn changed")
    );
    let mut bad = good.clone();
    bad.entries[0] = entry("F1", "F1 (T1): make is broken");
    assert_eq!(
        shape_problem(&earlier, &bad),
        Some("an earlier entry changed")
    );
    let mut bad = good.clone();
    bad.turns[1].number = 3;
    assert_eq!(
        shape_problem(&earlier, &bad),
        Some("the new turns are not numbered in order")
    );
    let mut bad = good.clone();
    bad.turns[1].tools = new_tools[..1].to_vec();
    assert_eq!(
        shape_problem(&earlier, &bad),
        Some("a new tool call has no line")
    );
    let mut bad = good.clone();
    bad.turn_count = 5;
    assert_eq!(
        shape_problem(&earlier, &bad),
        Some("the turn count does not match the turns")
    );
    let mut bad = good.clone();
    bad.entries[1] = entry("F1", "F1 (T2): again");
    assert_eq!(
        shape_problem(&earlier, &bad),
        Some("two entries share an ID")
    );
    let mut bad = good.clone();
    bad.entries[1] = entry("S1", "tests pass");
    assert_eq!(
        shape_problem(&earlier, &bad),
        Some("an entry does not start with its ID")
    );
    let mut bad = good;
    bad.turns.remove(0);
    assert_eq!(
        shape_problem(&earlier, &bad),
        Some("an earlier turn changed")
    );
}

#[test]
fn replaced_ids_and_highest_numbers_read_only_well_formed_ids() {
    assert_eq!(
        replaced_ids("S3 (T2): fixed; Replaces S1, S2 and D4.) replaces nothing, replaces F9"),
        ["S1", "S2", "D4", "F9"]
    );
    let entries = [
        entry("", ""),
        entry("F4", "F4 (T1): x"),
        entry("F18446744073709551616", "F18446744073709551616: overflow"),
        entry("R2", "R2: y"),
    ];
    assert_eq!(highest_ids(&entries), [2, 4, 0, 0, 0]);
    let highest = highest_ids(&entries);
    assert!(was_used("F3", &highest));
    assert!(!was_used("F5", &highest));
    assert!(!was_used("F0", &highest));
    assert!(!was_used("X1", &highest));
}
