use ofx_trace::{Ring, TraceContext};

use super::*;
use crate::compactor::trace::{CompactionEvent, CompactionTraceKind};

fn traced() -> (Tracer, &'static Ring<CompactionEvent>) {
    let ring: &'static Ring<CompactionEvent> = Box::leak(Box::new(Ring::new(64)));
    (Tracer::new(ring, TraceContext::default()), ring)
}

fn logged(ring: &Ring<CompactionEvent>) -> Vec<String> {
    ring.snapshot()
        .into_iter()
        .map(|event| {
            assert_eq!(event.event.kind, CompactionTraceKind::Log);
            assert!(!event.event.failed);
            event.event.detail
        })
        .collect()
}

fn entry(id: &str, text: &str) -> Entry {
    Entry {
        id: id.to_owned(),
        text: text.to_owned(),
    }
}

fn known(turns: &[usize], tools: &[usize], open: bool) -> Known {
    Known {
        turns: turns.to_vec(),
        tools: tools.to_vec(),
        open,
    }
}

fn read(reply: &str, known: &Known, earlier: &[Entry]) -> Written {
    super::read(reply, known, earlier, Tracer::detached())
}

fn candidates(messages: &[Message<'_>], filed: &[Entry]) -> Vec<Candidate> {
    super::candidates(messages, filed, Tracer::detached())
}

fn texts(entries: &[Entry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.text.as_str()).collect()
}

#[test]
fn a_saved_entry_with_an_empty_id_is_skipped_when_new_ids_are_numbered() {
    let mut text = String::new();
    write_highest_ids(
        &mut text,
        &highest_ids(&[entry("", ""), entry("F4", "F4 (T1): x")]),
    );
    assert_eq!(
        text,
        " The highest IDs so far: F4. Number new entries after them."
    );
}

#[test]
fn entries_are_found_by_their_ids_with_the_lines_that_continue_them() {
    let text = "Rules:\n- R1 (M1): \"never force push\"\n\nDecisions:\n- D1 (M3): Regex, not a Zig parser.\n  It is simpler.\n\nStatus:\n- S1 (M4): build passes\nOpen:\nO2: add a README";
    let found = items(text);
    assert_eq!(found.len(), 4);
    assert_eq!(found[0].id, "R1");
    assert_eq!(
        found[1].text,
        "- D1 (M3): Regex, not a Zig parser.\n  It is simpler."
    );
    assert_eq!(found[3].id, "O2");

    for line in [
        "**D12**: bold",
        "* F4) found",
        "- R1 (M1): \"never force push\"",
        "- S3 [T4, T5]: fixed",
        "- **F7** (T12) (M3): ran",
        "- R2(M1): no space",
    ] {
        assert!(item_id(line).is_some(), "{line}");
    }
    for line in [
        "- F2F meeting",
        "- S3 bucket",
        "Decisions:",
        "- D: none",
        "R12",
        "- S3 (the bucket) holds it",
        "- F4 (unclosed: x",
        "- E1: evidence is no longer a kind",
        "T12: a tool note",
    ] {
        assert!(item_id(line).is_none(), "{line}");
    }
}

#[test]
fn notes_are_read_per_turn_and_tool_call_and_entries_only_ever_add() {
    let reply = "## Turn 3
In between: Read the loader and found dates kept as text,
then fixed the parser.
- T8: read to find where dates are parsed
T9 (shell): ran the tests; 1 of 6 failed
T99: a tool call this compaction does not have

**Turn 4**
In between: none

Turn in progress
In between: started the release notes

Rules:
- R2 (M3): \"Store every price as integer cents.\"
- R3 (M4): \"keep it short\"
Facts: F4 (T8): dates were parsed as text
Decisions:
- D1 (M3): a rewrite of an entry that already exists
- D2 (M4): no cache; replaces D1
Status:
none";
    let earlier = [entry("D1", "D1 (M2): cache in a pickle file")];
    let written = read(reply, &known(&[3, 4], &[8, 9], true), &earlier);

    assert_eq!(
        written.work(3),
        "Read the loader and found dates kept as text, then fixed the parser."
    );
    assert_eq!(written.work(4), "");
    assert_eq!(written.work(0), "started the release notes");
    assert_eq!(written.tool(8), "read to find where dates are parsed");
    assert_eq!(written.tool(9), "ran the tests; 1 of 6 failed");
    assert_eq!(written.tool(99), "");
    assert_eq!(written.noted, [3, 4]);
    assert_eq!(written.unknown, 1);
    assert_eq!(written.repeated, 0);

    assert_eq!(
        texts(&written.entries),
        [
            "R2 (M3): \"Store every price as integer cents.\"",
            "R3 (M4): \"keep it short\"",
            "F4 (T8): dates were parsed as text",
            "D3 (M3): a rewrite of an entry that already exists",
            "D2 (M4): no cache; replaces D1",
        ]
    );
    assert_eq!(written.entries[2].id, "F4");

    let again = read(
        "Decisions:
- D1 (M2): cache in a pickle file
- D1 (M2): cache in a file (replaced by D2)
- D5 (M2): cache on disk (replaced by D6).
- D8 (M5): the old loader (replaced by a new one) stays for tests
- D4 (M5): keep the pickle cache",
        &known(&[5], &[], false),
        &earlier,
    );
    assert_eq!(
        texts(&again.entries),
        [
            "D8 (M5): the old loader (replaced by a new one) stays for tests",
            "D4 (M5): keep the pickle cache",
        ]
    );
    assert_eq!(again.repeated, 3);

    let summary_only = read(
        "The user fixed the build; the tests pass.",
        &known(&[], &[8], false),
        &earlier,
    );
    assert!(summary_only.works.is_empty());
    assert!(summary_only.entries.is_empty());
    assert_eq!(
        summary_only.earlier,
        "The user fixed the build; the tests pass."
    );

    let (trace, ring) = traced();
    let far = super::read(
        "Facts:\n- F18446744073709551615 (T8): the parser is slow\n- F1000 (T8): the loader is fast",
        &known(&[3], &[8], false),
        &[],
        trace,
    );
    assert_eq!(
        texts(&far.entries),
        [
            "F1001 (T8): the parser is slow",
            "F1000 (T8): the loader is fast"
        ]
    );
    assert_eq!(
        logged(ring),
        [
            "compaction entries renumbered because their IDs were taken or far above the highest count=1"
        ]
    );

    let closed = read(reply, &known(&[3], &[8], false), &[]);
    assert_eq!(closed.work(0), "");
    assert_eq!(closed.work(4), "");
}

#[test]
fn a_run_of_tool_calls_may_share_a_note_kept_on_its_first_call() {
    let reply = "Turn 2
In between: Traced the loader.
T1\u{2013}T4: located the loader and its callers
T5-T6: ran the tests
T7 to T8: none
T9 - read the config
T10\u{2013}T99: a run this compaction does not have

Turn 3
In between: Answered from memory.
T: none.";
    let written = read(
        reply,
        &known(&[2, 3], &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], false),
        &[],
    );
    assert_eq!(
        written.tool(1),
        "T1\u{2013}T4: located the loader and its callers"
    );
    assert_eq!(written.tool(2), "");
    assert_eq!(written.tool(5), "T5\u{2013}T6: ran the tests");
    assert_eq!(written.tool(7), "");
    assert_eq!(written.tool(9), "read the config");
    assert_eq!(written.tool(10), "");
    assert_eq!(written.unknown, 1);
    assert_eq!(written.work(3), "Answered from memory.");
}

#[test]
fn a_reply_in_none_of_the_asked_form_becomes_the_newest_turns_notes() {
    let prose = "The reads completed.\nKeep SENTINEL_42 in mind.";
    let complete = read(prose, &known(&[3, 4], &[7], false), &[]);
    assert_eq!(complete.works.len(), 1);
    assert_eq!(complete.work(4), prose);
    assert_eq!(complete.work(3), "");
    let running = read(prose, &known(&[3], &[], true), &[]);
    assert_eq!(running.work(0), prose);
    let misplaced = read(
        "Turn 9\nIn between: elsewhere",
        &known(&[3], &[], false),
        &[],
    );
    assert!(misplaced.works.is_empty());
    assert_eq!(misplaced.unknown, 1);
    let long = "word ".repeat(4000);
    let clipped = read(&long, &known(&[3], &[], false), &[]);
    assert_eq!(clipped.work(3).len(), MAX_UNREAD_BYTES);
}

#[test]
fn skills_and_mcp_tools_are_listed_from_their_calls_counted_in_the_order_first_used() {
    let earlier = [Used {
        kind: UsedKind::Skill,
        name: "skill:ab:4/fx-conventions".to_owned(),
        calls: 1,
        first_tool: 2,
        last_tool: 2,
    }];
    let call = |number, name, arguments| Call {
        number,
        name,
        arguments,
    };
    let calls = [
        call(7, "read_file", "{\"path\":\"a.zig\"}"),
        call(8, "mcp_linear_list_issues", "{}"),
        call(9, "skill", "{\"location\":\"skill:ab:4/fx-conventions\"}"),
        call(10, "mcp_select_tool", "{\"name\":\"mcp_linear_get_issue\"}"),
        call(11, "mcp_linear_list_issues", "{\"team\":\"fx\"}"),
        call(
            12,
            "mcp_features",
            "{\"action\":\"resource_read\",\"server\":\"linear\"}",
        ),
        call(
            13,
            "skill",
            "{\"location\":\"skill:ab:4/zig\",\"resource\":\"references/io.md\"}",
        ),
        call(14, "skill", "not json"),
    ];
    let used = add_used(&earlier, &calls);
    assert_eq!(used.len(), 4);
    assert_eq!(used[0].calls, 2);
    assert_eq!(used[0].last_tool, 9);
    assert_eq!(used[1].name, "mcp_linear_list_issues");
    assert_eq!(used[1].calls, 2);
    assert_eq!(used[1].first_tool, 8);
    assert_eq!(used[1].last_tool, 11);
    assert_eq!(used[2].name, "mcp_features linear resource_read");
    assert_eq!(used[3].kind, UsedKind::Skill);
    assert_eq!(used[3].name, "skill:ab:4/zig references/io.md");
}

#[test]
fn sentences_that_may_set_rules_are_found_in_the_users_messages_with_what_a_short_one_refers_to() {
    let message = |turn, text, in_progress| Message {
        turn,
        text,
        in_progress,
    };
    let messages = [
        message(
            2,
            "Before you start, some rules for this whole task: use only the Python standard library, no third-party packages. Never modify anything under src/, the tool only reads it. Say OK.",
            false,
        ),
        message(
            5,
            "Add an evaluator for integer literals. Never use eval() or exec() for this. Add tests and run them.",
            false,
        ),
        message(
            7,
            "so don't do fix 3 ?\n```\nnever run this in production\n```\n\u{2503} it never resolves custom themes\n> only quoted text",
            false,
        ),
        message(
            22,
            "What about a --diff option that compares the limits against another git revision? Actually no, that's too complex for now. Don\u{2019}t build it.",
            false,
        ),
        message(23, "Write the README in under 30 lines.", false),
        message(
            24,
            "Never modify anything under src/, the tool only reads it.",
            false,
        ),
        message(0, "Keep going, but don't touch the tests.", true),
    ];
    let found = candidates(&messages, &[]);
    let expected = [
        (
            2,
            "Before you start, some rules for this whole task: use only the Python standard library, no third-party packages.",
            false,
        ),
        (
            2,
            "Never modify anything under src/, the tool only reads it.",
            false,
        ),
        (5, "Never use eval() or exec() for this.", false),
        (
            22,
            "What about a --diff option that compares the limits against another git revision? Actually no, that's too complex for now. Don\u{2019}t build it.",
            false,
        ),
        (23, "Write the README in under 30 lines.", false),
        (0, "Keep going, but don't touch the tests.", true),
    ];
    let found_tuples: Vec<(usize, &str, bool)> = found
        .iter()
        .map(|candidate| {
            (
                candidate.turn,
                candidate.text.as_str(),
                candidate.in_progress,
            )
        })
        .collect();
    assert_eq!(found_tuples, expected);

    let mut text = String::new();
    write_candidates(&mut text, &found[1..2]);
    write_candidates(&mut text, &found[5..]);
    assert!(text.contains("File each one that still applies under Rules"));
    assert!(
        text.contains("- turn 2: \"Never modify anything under src/, the tool only reads it.\"\n")
    );
    assert!(text.contains("- turn in progress: \"Keep going, but don't touch the tests.\"\n"));

    let filed = [
        entry(
            "R1",
            "R1 (turn in progress): \u{201C}Keep going, but don't touch the tests.\u{201D}",
        ),
        entry(
            "F2",
            "F2 (M5): the user said \"Never use eval() or exec() for this.\"",
        ),
    ];
    let later = [messages[1], messages[6]];
    let unfiled = candidates(&later, &filed);
    assert_eq!(unfiled.len(), 1);
    assert_eq!(unfiled[0].text, "Never use eval() or exec() for this.");
}

#[test]
fn the_request_asks_only_for_the_new_turns_and_numbers_entries_after_the_highest() {
    let entries = [
        entry("R2", "R2 (M1): \"x\""),
        entry("F7", "F7 (T3): y"),
        entry("S1", "S1 (M2): z"),
    ];
    let heading = |number, first_tool, last_tool| Heading {
        number,
        first_tool,
        last_tool,
        ..Heading::default()
    };
    let turns = [heading(4, 10, 19), heading(5, 0, 0), heading(9, 20, 20)];
    let open = heading(0, 21, 23);
    let mut text = String::new();
    write_request(
        &mut text,
        &Asked {
            turns: &turns,
            open: Some(&open),
            highest: highest_ids(&entries),
            after_conversation: false,
        },
    );
    let headings = "without skipping any, each followed by its notes:\n\nTurn 4 (T10\u{2013}T19)\nTurn 5 (no tool calls)\nTurn 9 (T20)\nTurn in progress (T21\u{2013}T23)\n\nUnder each heading:\nIn between:";
    for part in [
        headings,
        "`T<number>:`",
        "For the turn in progress",
        "\nRules:\n",
        "\nFacts:\n",
        "\nDecisions:\n",
        "\nStatus:\n",
        "\nOpen:\n",
        "like `(turn <number>)`, or `(turn in progress)`",
        "will not be available later",
    ] {
        assert!(text.contains(part), "{part}");
    }
    assert!(text.contains(" The highest IDs so far: R2, F7, S1. Number new entries after them."));
    assert!(text.contains(
        "or answers or finishes an earlier open entry, end with \"replaces\" and that entry's ID."
    ));
    for example in ["T40", "M2", "replaces D", "replaces S"] {
        assert!(!text.contains(example), "{example}");
    }
    text.clear();
    write_request(
        &mut text,
        &Asked {
            turns: &[],
            open: None,
            highest: [0; ENTRY_KINDS.len()],
            after_conversation: false,
        },
    );
    assert!(text.contains(" Number each kind from 1."));
    assert!(text.contains("will not be available later"));
    assert!(!text.contains("in progress"));
    assert!(!text.contains("Under each heading"));
}

#[test]
fn a_heading_counts_with_its_tool_calls_or_a_title_after_it_but_not_inside_a_sentence() {
    for line in [
        "Turn 12",
        "[Turn 12]:",
        "Turn 12 (T40\u{2013}T45)",
        "Turn 12: fixing the loader",
        "Turn 12 \u{2013} fixing the loader",
        "Turn 12 - build",
    ] {
        assert_eq!(turn_number(line), Some(12), "{line}");
    }
    assert_eq!(turn_number("Turn in progress (T50\u{2013}T52)"), Some(0));
    for line in [
        "Turn 12 was slow",
        "Turn 12a",
        "Turn in progressive",
        "Turn 0",
        "Turns 1 to 3",
    ] {
        assert_eq!(turn_number(line), None, "{line}");
    }
}

#[test]
fn notes_cut_to_their_room_and_renumbered_entries_are_logged() {
    let (trace, ring) = traced();
    let long = "word ".repeat(4000);
    let clipped = super::read(&long, &known(&[3], &[], false), &[], trace);
    assert_eq!(clipped.work(3).len(), MAX_UNREAD_BYTES);
    let reply = format!(
        "Turn 3\nIn between: {}\nT8: {}\n\nEarlier summary: {}\n\nFacts:\n- F2 (T8): taken\n- F2 (T8): taken again",
        "w".repeat(MAX_WORK_BYTES + 5),
        "t".repeat(MAX_TOOL_NOTE_BYTES + 1),
        "e".repeat(MAX_EARLIER_BYTES + 3),
    );
    let written = super::read(&reply, &known(&[3], &[8], false), &[], trace);
    assert_eq!(written.earlier.len(), MAX_EARLIER_BYTES);
    assert_eq!(
        logged(ring),
        [
            "a compaction note was cut to 8192 bytes number=3 bytes=19999",
            "a compaction note was cut to 1200 bytes number=3 bytes=1205",
            "a compaction note was cut to 300 bytes number=8 bytes=301",
            "the summary of earlier compactions was cut to 2400 bytes bytes=2403",
            "compaction entries renumbered because their IDs were taken or far above the highest count=1",
        ]
    );
}

#[test]
fn rule_candidates_past_their_room_are_logged() {
    let (trace, ring) = traced();
    let rule =
        |index: usize| format!("Always keep the build green for module number {index:04} please.");
    let text: String = (0..400).map(|index| rule(index) + "\n").collect();
    let messages = [Message {
        turn: 1,
        text: &text,
        in_progress: false,
    }];
    let found = super::candidates(&messages, &[], trace);
    let bytes: usize = found.iter().map(|candidate| candidate.text.len()).sum();
    assert!(bytes <= MAX_CANDIDATE_BYTES);
    assert_eq!(
        logged(ring),
        [format!(
            "rule candidates over their room; the newest are left out candidates={} bytes={bytes}",
            found.len()
        )]
    );
}
