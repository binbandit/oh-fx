use ofx_contract::{ApprovalAnswer, ApprovalDecision, RequestId, UiCommand};

use super::super::test_shell::TestShell;
use super::tests::{ARMED_MS, approved, approving, editing, file_request, press};

const AMEND_HINT: &str = "1–3 choose now    ↑↓ options    tab amend    enter confirm    esc cancel";
const OPTIONS_HINT: &str = "1–3 choose now    ↑↓ or tab options    enter confirm    esc cancel";

fn answer(decision: ApprovalDecision, feedback: Option<&str>) -> UiCommand {
    UiCommand::Approval {
        request_id: RequestId::new(4),
        answer: ApprovalAnswer {
            decision,
            feedback: feedback.map(str::to_owned),
        },
    }
}

fn type_slowly(test: &mut TestShell, bytes: &[u8]) {
    press(test, bytes);
    test.advance(ARMED_MS);
}

#[test]
fn tab_on_yes_opens_a_draft_that_enter_sends_as_feedback() {
    let mut test = approving();
    let screen = test.screen();
    assert!(screen.contains(AMEND_HINT), "{screen}");
    press(&mut test, b"\t");
    let screen = test.screen();
    assert!(
        screen.contains("  ❯ 1. Yes, and tell oh-fx what to do next\n"),
        "{screen}"
    );
    assert!(screen.contains("    3. No\n"), "{screen}");
    type_slowly(&mut test, b"read the tests next");
    assert!(
        test.screen().contains("  ❯ 1. Yes, read the tests next \n"),
        "{}",
        test.screen()
    );
    assert!(test.shell.composer.is_empty());
    press(&mut test, b"\r");
    assert_eq!(
        test.sent().last(),
        Some(&answer(ApprovalDecision::Once, Some("read the tests next")))
    );
}

#[test]
fn tab_on_no_drafts_the_denial_and_digits_join_the_draft() {
    let mut test = approving();
    press(&mut test, b"\x1b[A");
    press(&mut test, b"\t");
    assert!(
        test.screen()
            .contains("  ❯ 3. No, and tell oh-fx what to do differently\n"),
        "{}",
        test.screen()
    );
    press(&mut test, b"keep 2 copies");
    assert!(!approved(&test));
    press(&mut test, b"\r");
    assert_eq!(
        test.sent().last(),
        Some(&answer(ApprovalDecision::Deny, Some("keep 2 copies")))
    );
}

#[test]
fn tab_on_the_remembering_choice_moves_on_and_offers_no_draft() {
    let mut test = approving();
    press(&mut test, b"\x1b[B");
    let screen = test.screen();
    assert!(screen.contains(OPTIONS_HINT), "{screen}");
    press(&mut test, b"\t");
    assert!(test.screen().contains("  ❯ 3. No\n"), "{}", test.screen());
    press(&mut test, b"\x1b[A");
    press(&mut test, b"\r");
    assert_eq!(
        test.sent().last(),
        Some(&answer(ApprovalDecision::Always, None))
    );
}

#[test]
fn moving_away_keeps_a_draft_that_its_choice_still_sends() {
    let mut test = approving();
    press(&mut test, b"\t");
    type_slowly(&mut test, b"then run the tests");
    press(&mut test, b"\x1b[B");
    let screen = test.screen();
    assert!(screen.contains("    1. Yes\n"), "{screen}");
    assert!(screen.contains("  ❯ 2. Yes"), "{screen}");
    press(&mut test, b"\x1b[A");
    assert!(test.screen().contains("  ❯ 1. Yes\n"), "{}", test.screen());
    press(&mut test, b"\r");
    assert_eq!(
        test.sent().last(),
        Some(&answer(ApprovalDecision::Once, Some("then run the tests")))
    );
}

#[test]
fn arrows_and_editing_keys_edit_the_draft_while_it_is_open() {
    let mut test = approving();
    press(&mut test, b"\t");
    press(&mut test, b"read notes");
    press(&mut test, b"\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D");
    press(&mut test, b"the ");
    press(&mut test, b"\x05!");
    press(&mut test, b"\x7f\x7f");
    press(&mut test, b"\x01\x0b");
    press(&mut test, b"start over");
    test.advance(ARMED_MS);
    press(&mut test, b"\r");
    assert_eq!(
        test.sent().last(),
        Some(&answer(ApprovalDecision::Once, Some("start over")))
    );
}

#[test]
fn left_and_right_move_the_choice_while_no_draft_is_open() {
    let mut test = approving();
    press(&mut test, b"\x1b[C");
    assert!(test.screen().contains("  ❯ 2. Yes"), "{}", test.screen());
    press(&mut test, b"\x1b[D\x1b[D");
    assert!(test.screen().contains("  ❯ 3. No\n"), "{}", test.screen());
}

#[test]
fn typing_in_the_draft_holds_off_yes_until_it_stops() {
    let mut test = approving();
    press(&mut test, b"\t");
    press(&mut test, b"quick\r");
    assert!(!approved(&test));
    test.advance(ARMED_MS);
    press(&mut test, b"\r");
    assert_eq!(
        test.sent().last(),
        Some(&answer(ApprovalDecision::Once, Some("quick")))
    );
}

#[test]
fn ctrl_c_denies_without_the_draft() {
    let mut test = approving();
    press(&mut test, b"\t");
    press(&mut test, b"never mind");
    press(&mut test, b"\x03");
    assert_eq!(
        test.sent().last(),
        Some(&answer(ApprovalDecision::Deny, None))
    );
}

#[test]
fn a_paste_into_an_open_draft_joins_it_on_one_line() {
    let mut test = approving();
    press(&mut test, b"\t");
    press(&mut test, b"\x1b[200~first\nsecond\x1b[201~");
    assert!(!approved(&test));
    assert!(test.shell.composer.is_empty());
    test.advance(ARMED_MS);
    press(&mut test, b"\r");
    assert_eq!(
        test.sent().last(),
        Some(&answer(ApprovalDecision::Once, Some("first second")))
    );
}

#[test]
fn a_file_review_drafts_feedback_after_its_own_labels() {
    let mut test = editing();
    test.deliver(file_request(
        4,
        "edit_file",
        "docs/notes.md",
        Some(b"alpha\nbeta\n"),
        b"alpha\nBETA\n",
    ));
    test.screen();
    test.advance(ARMED_MS);
    press(&mut test, b"\t");
    assert!(
        test.screen()
            .contains("  ❯ 1  Apply once, and tell oh-fx what to do next\n"),
        "{}",
        test.screen()
    );
    press(&mut test, b"\x1b[A\t");
    assert!(
        test.screen()
            .contains("  ❯ 3  Don't apply, and tell oh-fx what to do differently\n"),
        "{}",
        test.screen()
    );
    press(&mut test, b"keep beta lowercase");
    press(&mut test, b"\r");
    assert_eq!(
        test.sent().last(),
        Some(&answer(ApprovalDecision::Deny, Some("keep beta lowercase")))
    );
}

#[test]
fn a_long_draft_keeps_its_cursor_in_view() {
    let mut test = approving();
    press(&mut test, b"\t");
    let long = "x".repeat(200);
    press(&mut test, long.as_bytes());
    press(&mut test, b"END");
    let screen = test.screen();
    let row = screen
        .lines()
        .find(|line| line.starts_with("  ❯ 1. Yes, "))
        .unwrap_or_else(|| panic!("{screen}"));
    assert!(row.ends_with("xEND "), "{row:?}");
    assert!(
        ofx_text::visible_width(row) <= 80,
        "{row:?} is wider than the terminal"
    );
}
