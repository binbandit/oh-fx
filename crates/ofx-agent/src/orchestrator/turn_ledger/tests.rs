use super::*;

fn ledger(records: &[TurnRecord]) -> TurnLedger {
    TurnLedger {
        records: records.to_vec(),
    }
}

fn cut(turns: usize, tool_steps: usize) -> Cut {
    Cut {
        turns,
        tool_steps,
        ..Cut::default()
    }
}

#[test]
fn a_cut_counts_only_the_turns_the_log_holds() {
    let turns = ledger(&[TurnRecord::Unsaved, TurnRecord::Saved, TurnRecord::Saved]);
    assert_eq!(
        turns.logged_cut(cut(2, 0)),
        HistoryCut {
            turns: 1,
            tool_steps: 0
        }
    );
    assert_eq!(
        turns.logged_cut(cut(3, 2)),
        HistoryCut {
            turns: 2,
            tool_steps: 2
        }
    );
}

#[test]
fn steps_of_an_unsaved_turn_are_never_counted_in_the_log() {
    let turns = ledger(&[TurnRecord::Saved, TurnRecord::Unsaved, TurnRecord::Saved]);
    assert_eq!(
        turns.logged_cut(cut(1, 3)),
        HistoryCut {
            turns: 1,
            tool_steps: 0
        }
    );
}

#[test]
fn a_turn_only_the_log_holds_is_covered_with_the_turns_before_the_cut() {
    let turns = ledger(&[
        TurnRecord::Saved,
        TurnRecord::LogOnly,
        TurnRecord::Saved,
        TurnRecord::LogOnly,
    ]);
    assert_eq!(
        turns.logged_cut(cut(1, 0)),
        HistoryCut {
            turns: 2,
            tool_steps: 0
        }
    );
    assert_eq!(
        turns.logged_cut(cut(1, 1)),
        HistoryCut {
            turns: 2,
            tool_steps: 1
        }
    );
    assert_eq!(
        turns.logged_cut(cut(2, 0)),
        HistoryCut {
            turns: 4,
            tool_steps: 0
        }
    );
}

#[test]
fn compacting_drops_the_covered_records_and_keeps_the_rest_in_order() {
    let mut turns = ledger(&[
        TurnRecord::Unsaved,
        TurnRecord::LogOnly,
        TurnRecord::Saved,
        TurnRecord::Unsaved,
    ]);
    turns.compact(cut(1, 0));
    assert_eq!(turns.records, [TurnRecord::Saved, TurnRecord::Unsaved]);
    turns.compact(cut(1, 2));
    assert_eq!(turns.records, [TurnRecord::Unsaved]);
    turns.reset(2);
    assert_eq!(turns.records, [TurnRecord::Saved, TurnRecord::Saved]);
}
