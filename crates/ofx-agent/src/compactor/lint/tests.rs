use super::*;
use crate::compactor::checkpoint::ENTRY_KINDS;

fn record(number: usize, text: &str, failed: bool) -> Record {
    Record {
        number,
        text: text.to_owned(),
        failed,
    }
}

fn entry(id: &str, text: &str) -> Entry {
    Entry {
        id: id.to_owned(),
        text: text.to_owned(),
    }
}

fn note(number: usize, text: &str) -> Note {
    Note {
        number,
        text: text.to_owned(),
    }
}

const USERS: [&str; 1] = ["Never push to main. Keep the fix small."];

struct Fixture {
    tools: Vec<Record>,
    turns: Vec<Record>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            tools: vec![
                record(
                    7,
                    "T7 shell: zig build test\nResult:\n{\"exit_code\":1,\"output\":\"2 of 141 tests failed in src/core/app.zig\"}",
                    true,
                ),
                record(
                    8,
                    "T8 read_file: src/core/app.zig\nResult:\nconst max_bytes = 16_384;\nfn resumeForWrite() void {}",
                    false,
                ),
                record(
                    9,
                    "T9 shell: git log\nResult:\ncommit 707afac508e1 bumped libfx to 0.0.10",
                    false,
                ),
            ],
            turns: vec![record(
                4,
                "M4 turn: fix the tests\nUser 4:\nNever push to main. Keep the fix small.\n",
                false,
            )],
        }
    }

    fn sources(&self, highest: Highest) -> Sources<'_> {
        Sources {
            turn_count: 4,
            tool_count: 9,
            turns: &self.turns,
            tools: &self.tools,
            users: &USERS,
            kept: &[],
            highest,
        }
    }

    fn checked(&self, written: Written, earlier: &[Entry]) -> Written {
        self.counted(written, earlier).0
    }

    fn counted(&self, written: Written, earlier: &[Entry]) -> (Written, Counts) {
        let mut counts = Counts::default();
        let result = check(
            written,
            earlier,
            &self.sources([0; ENTRY_KINDS.len()]),
            &mut counts,
        );
        (result, counts)
    }
}

fn with_entries(entries: &[Entry]) -> Written {
    Written {
        entries: entries.to_vec(),
        ..Written::default()
    }
}

fn texts(entries: &[Entry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.text.as_str()).collect()
}

#[test]
fn entries_that_name_their_source_and_state_what_it_holds_pass_unmarked() {
    let entries = [
        entry("F1", "F1 (T7): 2 of 141 tests fail in `src/core/app.zig`"),
        entry(
            "F2",
            "F2 (T8, T9): max_bytes is 16_384; libfx is at 0.0.10 since commit 707afac508e1",
        ),
        entry("F3", "F3 (M4): resumeForWrite lives in src/core/app.zig"),
        entry("R1", "R1 (M4): \"never push to main\""),
        entry("R2", "R2 (turn in progress): \"keep the fix small\""),
        entry("R3", "R3 (turn 4): \"never push to main\""),
        entry("S1", "S1 (T7): 141 tests run, 2 fail; replaces S0"),
    ];
    let earlier = [entry("S0", "S0 (M1): not started")];
    let (result, counts) = Fixture::new().counted(with_entries(&entries), &earlier);
    assert_eq!(texts(&result.entries), texts(&entries));
    assert_eq!(counts.marked, 0);
}

#[test]
fn what_an_entry_gets_wrong_is_marked_and_the_entry_stays() {
    let entries = [
        entry("F1", "F1: the suite has 141 tests"),
        entry("F2", "F2 (T7): 3 of 142 tests fail in src/core/main.zig"),
        entry("F3", "F3 (T99): the release is out"),
        entry("D1", "D1 (M4): keep the fix small; replaces D7"),
        entry("R1", "R1 (M4): \"never force push\""),
        entry("R2", "R2 (M4): keep it small"),
        entry(
            "F4",
            "F4 (T9): LIBFX went to 0.0.10-dev after ~707AFAC508E1",
        ),
        entry("F5", "F5 (turn 9): 141 tests"),
    ];
    let (result, counts) = Fixture::new().counted(with_entries(&entries), &[]);
    assert_eq!(
        texts(&result.entries),
        [
            "F1: the suite has 141 tests [check: no source]",
            "F2 (T7): 3 of 142 tests fail in src/core/main.zig [check: not in the saved turns or tool calls: 142, src/core/main.zig]",
            "F3 (T99): the release is out [check: T99 does not exist]",
            "D1 (M4): keep the fix small; replaces D7 [check: replaces D7, which does not exist]",
            "R1 (M4): \"never force push\" [check: not the user's exact words]",
            "R2 (M4): keep it small [check: no quote of the user's words]",
            "F4 (T9): LIBFX went to 0.0.10-dev after ~707AFAC508E1",
            "F5 (turn 9): 141 tests [check: M9 does not exist]",
        ]
    );
    assert_eq!(
        counts,
        Counts {
            marked: 7,
            no_source: 1,
            missing_ids: 2,
            unfound_values: 2,
            bad_replaces: 1,
            unquoted: 2,
            failed_as_success: 0,
        }
    );
}

#[test]
fn every_entry_names_its_source_and_one_about_a_failed_call_does_not_call_it_a_success() {
    let entries = [
        entry("D1", "D1: keep the fix small"),
        entry("S1", "S1: the tests run"),
        entry("O1", "O1: which branch ships it?"),
        entry("F1", "F1 (T7): the tests passed"),
        entry("S2", "S2 (M4, T7): the suite is green"),
        entry("S3", "S3 (T7, T8): the suite passes after the fix"),
        entry("S4", "S4 (T7\u{2013}T9): tests pass"),
    ];
    let result = Fixture::new().checked(with_entries(&entries), &[]);
    assert_eq!(
        texts(&result.entries),
        [
            "D1: keep the fix small [check: no source]",
            "S1: the tests run [check: no source]",
            "O1: which branch ships it? [check: no source]",
            "F1 (T7): the tests passed [check: T7 failed]",
            "S2 (M4, T7): the suite is green [check: T7 failed]",
            entries[5].text.as_str(),
            entries[6].text.as_str(),
        ]
    );
}

#[test]
fn an_entry_may_replace_one_written_before_it_in_the_same_reply() {
    let fixture = Fixture::new();
    let entries = [
        entry("S1", "S1 (T7): tests fail"),
        entry("S2", "S2 (T7): still failing, replaces S1"),
        entry("S3", "S3 (T7): failing; replaces S4"),
    ];
    let result = fixture.checked(with_entries(&entries), &[]);
    assert_eq!(result.entries[1].text, entries[1].text);
    assert!(
        result.entries[2]
            .text
            .ends_with("[check: replaces S4, which does not exist]")
    );

    let numbered = check(
        with_entries(&entries[2..]),
        &[],
        &fixture.sources([0, 0, 0, 4, 0]),
        &mut Counts::default(),
    );
    assert_eq!(numbered.entries[0].text, entries[2].text);
}

#[test]
fn tool_notes_are_checked_against_their_call_and_a_failed_call_is_not_a_success() {
    let fixture = Fixture::new();
    let notes = [
        note(7, "ran the tests; all 141 pass"),
        note(8, "read src/core/app.zig; max_bytes is 16_384"),
        note(9, "found libfx 0.0.11"),
    ];
    let written = Written {
        tools: notes.to_vec(),
        ..Written::default()
    };
    let result = fixture.checked(written, &[]);
    assert_eq!(
        result.tools[0].text,
        "ran the tests; all 141 pass [check: T7 failed]"
    );
    assert_eq!(result.tools[1].text, notes[1].text);
    assert_eq!(
        result.tools[2].text,
        "found libfx 0.0.11 [check: not in the saved turns or tool calls: 0.0.11]"
    );

    let honest = Written {
        tools: vec![note(7, "ran the tests; they did not pass")],
        ..Written::default()
    };
    assert_eq!(
        fixture.checked(honest, &[]).tools[0].text,
        "ran the tests; they did not pass"
    );
}

#[test]
fn a_note_shared_by_a_run_of_calls_is_checked_against_the_whole_run() {
    let shared = note(7, "T7\u{2013}T9: found src/core/app.zig and libfx 0.0.10");
    let written = Written {
        tools: vec![shared.clone()],
        ..Written::default()
    };
    assert_eq!(
        Fixture::new().checked(written, &[]).tools[0].text,
        shared.text
    );
}

#[test]
fn a_value_named_under_the_wrong_call_passes_one_found_nowhere_is_marked() {
    let fixture = Fixture::new();
    let notes = [
        note(
            4,
            "Read core/app.zig:12 (T9) and bumped libfx to 0.0.10; edited lib/other.ts",
        ),
        note(
            4,
            "Read /repo/src/core/app.zig:40 and `resumeForWrite(void)`; the user wants \"exact CLI parity\"",
        ),
    ];
    let written = Written {
        works: notes.to_vec(),
        ..Written::default()
    };
    let result = fixture.checked(written, &[]);
    assert_eq!(
        result.works[0].text,
        format!(
            "{} [check: not in the saved turns or tool calls: lib/other.ts]",
            notes[0].text
        )
    );
    assert_eq!(
        result.works[1].text,
        format!(
            "{} [check: not in the saved turns or tool calls: exact CLI parity]",
            notes[1].text
        )
    );

    let entries = [entry("F9", "F9 (T2): lib/other.ts holds the cache")];
    let earlier_only = fixture.checked(with_entries(&entries), &[]);
    assert_eq!(earlier_only.entries[0].text, entries[0].text);
}

#[test]
fn values_are_the_words_whose_exact_form_matters() {
    assert_eq!(
        values(
            "Ran `zig build test` on main.zig (T12) and M3: 141 passed in 12.3s, e.g. see ~/src/fx/build.zig, and/or \"the ledger\"; 2 of 12 failed."
        ),
        [
            "zig build test",
            "the ledger",
            "main.zig",
            "141",
            "12.3s",
            "~/src/fx/build.zig"
        ]
    );
    assert!(
        values("F12 T3\u{2013}T9 M40 R2 search/edit/read (T47,T60,T109) (M9/T254):").is_empty()
    );
    assert_eq!(values("see T3/main.zig").len(), 1);
}

#[test]
fn curly_quotes_are_checked_like_straight_ones() {
    let users = [normalized("please use only the standard library")];
    for (rule, marked) in [
        (
            "R1 (M2): \u{201c}Use only the standard library\u{201d}",
            false,
        ),
        ("R1 (M2): \u{201c}use no packages\u{201d}", true),
        ("R1 (M2): \"use only the standard library.\"", false),
    ] {
        let mut problems = Problems::default();
        check_quote(&mut problems, rule, &users, &mut Counts::default());
        assert_eq!(!problems.text.is_empty(), marked, "{rule}");
    }
}

#[test]
fn punctuation_inside_closing_quote_marks_is_not_part_of_the_quoted_words() {
    let texts = ["Plus a \"smaller documented limits\" section. It builds immediately."];
    for quoted in [
        "\u{201c}smaller documented limits,\u{201d}",
        "\"smaller documented limits.\"",
        "`smaller documented limits`",
    ] {
        let found = values(quoted);
        assert_eq!(found.len(), 1, "{quoted}");
        assert!(found_in(found[0], &texts), "{quoted}");
    }
    let paraphrase = values("the \u{201c}build immediately\u{201d} instruction");
    assert!(!found_in(paraphrase[0], &texts));
}

#[test]
fn citations_read_single_ids_and_runs() {
    let citation = |kind, first, last| Citation { kind, first, last };
    assert_eq!(
        citations("F3 (T40, M2): see T3\u{2013}T8, T10-T12 and T5 to T6; not T4x or ATM9"),
        [
            citation(b'T', 40, 40),
            citation(b'M', 2, 2),
            citation(b'T', 3, 8),
            citation(b'T', 10, 12),
            citation(b'T', 5, 6),
        ]
    );
}
