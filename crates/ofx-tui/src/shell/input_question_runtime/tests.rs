use std::path::PathBuf;

use ofx_contract::{
    QuestionBatchEntry, QuestionOption, QuestionRequest, RequestId, SkillMenuFocus, SkillMenuGroup,
    SkillMenuItem, SkillMenuSource, TurnId, TurnOutcome, UiCommand, UiEvent,
};

use super::super::test_shell::TestShell;

const HINT: &str = "1–4 choose now    ↑↓ options    tab questions    enter answer    esc cancel";

fn option(label: &str, description: Option<&str>) -> QuestionOption {
    QuestionOption {
        label: label.to_owned(),
        description: description.map(str::to_owned),
    }
}

fn entry(question: &str, labels: &[&str]) -> QuestionBatchEntry {
    QuestionBatchEntry {
        question: question.to_owned(),
        options: labels.iter().map(|label| option(label, None)).collect(),
    }
}

fn requested(turn: u64, id: u64, entries: Vec<QuestionBatchEntry>) -> UiEvent {
    UiEvent::QuestionRequested {
        turn_id: TurnId::new(turn),
        request: QuestionRequest {
            id: RequestId::new(id),
            entries,
        },
    }
}

fn proceed() -> Vec<QuestionBatchEntry> {
    vec![QuestionBatchEntry {
        question: "Should we proceed?".to_owned(),
        options: vec![
            option("Yes", Some("go ahead")),
            option("No", None),
            option("Maybe", Some("decide later")),
        ],
    }]
}

fn asking(entries: Vec<QuestionBatchEntry>) -> TestShell {
    let mut test = TestShell::start();
    test.submit("pick for me");
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(1),
    });
    test.deliver(requested(1, 4, entries));
    test
}

fn press(test: &mut TestShell, bytes: &[u8]) {
    test.type_bytes(bytes);
    test.step();
}

fn answered(id: u64, answers: Option<&[&str]>) -> UiCommand {
    UiCommand::QuestionAnswered {
        request_id: RequestId::new(id),
        answers: answers.map(|answers| answers.iter().map(|answer| (*answer).to_owned()).collect()),
    }
}

fn answers(test: &TestShell) -> Vec<UiCommand> {
    test.sent()
        .into_iter()
        .filter(|command| matches!(command, UiCommand::QuestionAnswered { .. }))
        .collect()
}

#[test]
fn a_question_replaces_the_composer_until_a_number_answers_it() {
    let mut test = asking(proceed());
    let screen = test.screen();
    for line in [
        "  Should we proceed?",
        "    1) Yes",
        "go ahead",
        "    2) No",
        "    3) Maybe",
        "    4) Other",
        HINT,
    ] {
        assert!(screen.contains(line), "{line}\n{screen}");
    }
    assert!(!screen.contains("auto · model-a"), "{screen}");
    assert!(!screen.contains("Thinking"), "{screen}");
    press(&mut test, b"2");
    assert_eq!(answers(&test), [answered(4, Some(&["No"]))]);
    let screen = test.screen();
    assert!(!screen.contains(HINT), "{screen}");
    assert!(
        screen.contains("  1) Should we proceed?\n     No"),
        "{screen}"
    );
    assert!(screen.contains("auto · model-a"), "{screen}");
    assert!(screen.contains("Thinking"), "{screen}");
}

#[test]
fn arrows_move_the_choice_tab_pages_left_goes_back_and_enter_answers() {
    let mut test = asking(vec![
        entry("Which depth?", &["Thorough", "Fast"]),
        entry("Ship it?", &["Yes", "No"]),
    ]);
    let screen = test.screen();
    assert!(!screen.contains("Question 1 of 2"), "{screen}");
    test.resize(24, 100);
    let screen = test.screen();
    assert!(screen.contains("Question 1 of 2"), "{screen}");
    press(&mut test, b"\t");
    assert!(test.screen().contains("Ship it?"));
    press(&mut test, b"\x1b[D");
    assert!(test.screen().contains("Which depth?"));
    press(&mut test, b"\x1b[B");
    press(&mut test, b"\r");
    let screen = test.screen();
    assert!(screen.contains("Ship it?"), "{screen}");
    assert!(screen.contains("Question 2 of 2"), "{screen}");
    for _ in 0..3 {
        press(&mut test, b"\x1b[A");
    }
    press(&mut test, b"\r");
    assert_eq!(answers(&test), [answered(4, Some(&["Fast", "Yes"]))]);
    let screen = test.screen();
    for line in [
        "  1) Which depth?",
        "     Fast",
        "  2) Ship it?",
        "     Yes",
    ] {
        assert!(screen.contains(line), "{line}\n{screen}");
    }
}

#[test]
fn the_other_choice_takes_a_typed_answer_with_digits_and_edits() {
    let mut test = asking(proceed());
    press(&mut test, b"4");
    assert!(answers(&test).is_empty());
    let screen = test.screen();
    assert!(!screen.contains("4) Other"), "{screen}");
    assert!(
        screen.contains("↑↓ cursor · shift+↑↓ options · tab questions"),
        "{screen}"
    );
    press(&mut test, b"wait 2 days");
    press(&mut test, b"\x7f\x7f\x7f\x7f");
    press(&mut test, "é".as_bytes());
    press(&mut test, b"\x1b[D");
    press(&mut test, b"\x1b[3~");
    press(&mut test, b"\x01");
    press(&mut test, b"please ");
    assert!(
        test.screen().contains("4) please wait 2 "),
        "{}",
        test.screen()
    );
    press(&mut test, b"\r");
    assert_eq!(answers(&test), [answered(4, Some(&["please wait 2 "]))]);
}

#[test]
fn shift_arrows_leave_the_typed_answer_and_keep_it_for_later() {
    let mut test = asking(proceed());
    press(&mut test, b"\x1b[A");
    press(&mut test, b"draft");
    press(&mut test, b"\x1b[1;2A");
    let screen = test.screen();
    assert!(screen.contains("4) draft"), "{screen}");
    assert!(screen.contains(HINT), "{screen}");
    press(&mut test, b"x");
    press(&mut test, b"\x1b[1;2B");
    press(&mut test, b"!");
    press(&mut test, b"\r");
    assert_eq!(answers(&test), [answered(4, Some(&["draft!"]))]);
}

#[test]
fn escape_cancels_the_question_and_the_turn_with_one_cancelled_row() {
    let mut test = asking(proceed());
    test.screen();
    press(&mut test, b"\x1b");
    test.advance(100);
    test.step();
    let sent = test.sent();
    assert!(
        sent.contains(&UiCommand::Cancel {
            turn_id: TurnId::new(1)
        }),
        "{sent:?}"
    );
    assert_eq!(answers(&test), [answered(4, None)]);
    let screen = test.screen();
    assert!(screen.contains("■ Cancelled"), "{screen}");
    assert!(
        !screen.contains("What can oh-fx do differently?"),
        "{screen}"
    );
    assert!(!screen.contains("Should we proceed?"), "{screen}");
    test.deliver(UiEvent::TurnFinished {
        turn_id: TurnId::new(1),
        outcome: TurnOutcome::Interrupted,
    });
    assert_eq!(test.screen().matches("■ Cancelled").count(), 1);
}

#[test]
fn ctrl_c_cancels_at_once_even_with_a_typed_answer() {
    let mut test = asking(proceed());
    press(&mut test, b"4typed");
    press(&mut test, b"\x03");
    assert_eq!(answers(&test), [answered(4, None)]);
    assert!(test.sent().contains(&UiCommand::Cancel {
        turn_id: TurnId::new(1)
    }));
    assert!(!test.shell.should_exit);
}

#[test]
fn escape_on_a_typed_answer_arms_a_clear_before_it_cancels() {
    let mut test = asking(proceed());
    press(&mut test, b"4typed");
    press(&mut test, b"\x1b");
    test.advance(100);
    test.step();
    assert!(answers(&test).is_empty());
    assert!(test.screen().contains("esc again to clear"));
    press(&mut test, b"\x1b");
    test.advance(100);
    test.step();
    assert!(answers(&test).is_empty());
    let screen = test.screen();
    assert!(!screen.contains("typed"), "{screen}");
    press(&mut test, b"\x1b");
    test.advance(100);
    test.step();
    assert_eq!(answers(&test), [answered(4, None)]);
}

#[test]
fn long_questions_and_choices_wrap_inside_the_footer() {
    let mut test = asking(vec![QuestionBatchEntry {
        question: format!(
            "{} end-of-question?",
            "Which verification steps should run".repeat(3)
        ),
        options: vec![
            option(
                "Run the complete verification suite before pushing",
                Some("Covers every crate and the end to end tests too"),
            ),
            option("Skip", None),
        ],
    }]);
    let screen = test.screen();
    for piece in [
        "end-of-question?",
        "1) Run the complete",
        "verification suite before",
        "end to end",
        "tests too",
        "2) Skip",
        "3) Other",
    ] {
        assert!(screen.contains(piece), "{piece}\n{screen}");
    }
    for line in screen.lines() {
        assert!(!line.contains('…'), "{screen}");
    }
}

#[test]
fn hostile_question_text_never_reaches_the_terminal_raw() {
    let mut test = asking(vec![QuestionBatchEntry {
        question: "Pick\u{1b}]2;PWNED\u{7}\u{1b}[2J one\u{202e}".to_owned(),
        options: vec![
            option("\u{1b}[31mred", Some("\u{9b}31mdesc")),
            option("ok", None),
        ],
    }]);
    let written = test.written();
    assert!(!written.contains("\x1b]2;PWNED"), "{written:?}");
    assert!(!written.contains('\u{7}'), "{written:?}");
    assert!(!written.contains('\u{202e}'), "{written:?}");
    assert!(!written.contains('\u{9b}'), "{written:?}");
    let screen = test.screen();
    assert!(
        screen.contains("Pick\\x1b]2;PWNED\\x07\\x1b[2J one\\u{202e}"),
        "{screen}"
    );
    assert!(screen.contains("1) \\x1b[31mred"), "{screen}");
    press(&mut test, b"1");
    assert_eq!(answers(&test), [answered(4, Some(&["\\x1b[31mred"]))]);
}

#[test]
fn pastes_reach_the_typed_answer_only_while_it_is_selected() {
    let mut test = asking(proceed());
    press(&mut test, b"\x1b[200~1\r\x1b[201~");
    assert!(answers(&test).is_empty());
    assert!(test.shell.composer.is_empty());
    press(&mut test, b"4");
    press(&mut test, b"\x1b[200~line one\r\nline\ttwo\x1b[201~");
    let screen = test.screen();
    assert!(screen.contains("4) line one"), "{screen}");
    assert!(screen.contains("line two"), "{screen}");
    press(&mut test, b"\r");
    assert_eq!(answers(&test), [answered(4, Some(&["line one\nline two"]))]);
    let screen = test.screen();
    assert!(screen.contains("     line one\n     line two"), "{screen}");
}

#[test]
fn questions_for_a_turn_that_is_not_shown_are_answered_with_nothing() {
    let mut test = TestShell::start();
    test.deliver(requested(3, 9, proceed()));
    assert_eq!(answers(&test), [answered(9, None)]);
    assert!(!test.screen().contains("Should we proceed?"));
}

#[test]
fn a_finished_turn_or_a_newer_question_never_leaves_one_waiting() {
    let mut test = asking(proceed());
    test.deliver(requested(1, 5, vec![entry("Second?", &["A", "B"])]));
    assert_eq!(answers(&test), [answered(4, None)]);
    let screen = test.screen();
    assert!(screen.contains("Second?"), "{screen}");
    test.deliver(UiEvent::TurnFinished {
        turn_id: TurnId::new(1),
        outcome: TurnOutcome::Completed,
    });
    let screen = test.screen();
    assert!(!screen.contains("Second?"), "{screen}");
    assert!(screen.contains("auto · model-a"), "{screen}");
    press(&mut test, b"x");
    assert_eq!(test.shell.composer.text(), "x");
}

#[test]
fn the_full_access_warning_gives_way_to_the_question_hint() {
    let mut test = TestShell::start_with(|options| {
        options.permission_mode = ofx_contract::PermissionMode::Yolo;
        options.full_access_warning = true;
    });
    test.submit("pick for me");
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(1),
    });
    test.deliver(requested(1, 4, proceed()));
    let screen = test.screen();
    assert!(screen.contains(HINT), "{screen}");
    assert!(!screen.contains("Full access enabled"), "{screen}");
}

#[test]
fn the_question_hint_outranks_an_armed_ctrl_c_exit() {
    let mut test = TestShell::start();
    test.submit("pick for me");
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(1),
    });
    let now_ms = test.shell.now_ms();
    test.shell.gestures.press_ctrl_c_exit(now_ms);
    test.deliver(requested(1, 4, proceed()));
    let screen = test.screen();
    assert!(screen.contains(HINT), "{screen}");
    assert!(!screen.contains("press ctrl+c again to exit"), "{screen}");
    press(&mut test, b"\x03");
    assert_eq!(answers(&test), [answered(4, None)]);
    assert!(!test.shell.should_exit);
}

fn paste(test: &mut TestShell, pasted: &str) {
    press(test, b"\x1b[200~");
    for chunk in pasted.as_bytes().chunks(512) {
        press(test, chunk);
    }
    press(test, b"\x1b[201~");
}

const LIMIT_NOTICE: &str = "That edit exceeds the input limit and was not applied.";

#[test]
fn typed_and_pasted_answers_stop_at_the_decision_input_limit() {
    let mut test = asking(proceed());
    press(&mut test, b"4");
    paste(&mut test, &"a".repeat(4097));
    assert!(test.written().contains(LIMIT_NOTICE));
    paste(&mut test, &"a".repeat(4095));
    press(&mut test, b"b");
    press(&mut test, b"c");
    assert!(test.written().contains(LIMIT_NOTICE));
    press(&mut test, b"\r");
    let expected = format!("{}b", "a".repeat(4095));
    assert_eq!(answers(&test), [answered(4, Some(&[expected.as_str()]))]);
}

#[test]
fn a_paste_meant_for_an_answer_never_reaches_the_composer() {
    let mut test = asking(proceed());
    press(&mut test, b"4");
    press(&mut test, b"\x1b[200~orphan");
    test.deliver(UiEvent::TurnFinished {
        turn_id: TurnId::new(1),
        outcome: TurnOutcome::Completed,
    });
    press(&mut test, b"\x1b[201~");
    assert!(
        test.shell.composer.is_empty(),
        "{}",
        test.shell.composer.text()
    );
}

#[test]
fn an_armed_clear_survives_cursor_keys_until_the_second_escape() {
    let mut test = asking(proceed());
    press(&mut test, b"4typed");
    press(&mut test, b"\x1b");
    test.advance(50);
    test.step();
    press(&mut test, b"\x1b[D");
    assert!(test.screen().contains("esc again to clear"));
    press(&mut test, b"\x1b");
    test.advance(50);
    test.step();
    assert!(answers(&test).is_empty());
    let screen = test.screen();
    assert!(!screen.contains("typed"), "{screen}");
    assert!(!screen.contains("esc again to clear"), "{screen}");
}

#[test]
fn focused_editor_keys_edit_only_the_typed_answer() {
    let mut test = asking(vec![entry("Continue?", &["Alpha"])]);
    test.shell
        .composer
        .insert_text("hidden composer", usize::MAX);
    press(&mut test, b"2");
    press(&mut test, b"alpha beta gamma");
    press(&mut test, b"\x01\x05\x02\x06\x17");
    press(&mut test, b"!");
    press(&mut test, b"\x15");
    press(&mut test, b"alpha beta");
    press(&mut test, b"\x01");
    press(&mut test, b"\x1bd");
    press(&mut test, b"\x05");
    press(&mut test, b"\x1b\x7f");
    press(&mut test, b"left right");
    press(&mut test, b"\x01\x06\x0b");
    press(&mut test, b"\r");
    assert_eq!(answers(&test), [answered(4, Some(&["l"]))]);
    assert_eq!(test.shell.composer.text(), "hidden composer");
}

#[test]
fn csi_u_digits_neither_choose_nor_type() {
    for sequence in [&b"\x1b[49;5u"[..], b"\x1b[\x1b[49;5u"] {
        let mut test = asking(vec![entry("Continue?", &["Alpha", "Beta", "Gamma"])]);
        press(&mut test, b"\x1b[B");
        press(&mut test, sequence);
        test.advance(100);
        test.step();
        press(&mut test, b"\t");
        assert!(answers(&test).is_empty(), "{sequence:?}");
        press(&mut test, b"\r");
        assert_eq!(
            answers(&test),
            [answered(4, Some(&["Beta"]))],
            "{sequence:?}"
        );
    }
    let mut test = asking(vec![entry("Continue?", &["Alpha", "Beta"])]);
    press(&mut test, b"3");
    press(&mut test, b"\x1b[49;5u");
    press(&mut test, b"x\r");
    assert_eq!(answers(&test), [answered(4, Some(&["x"]))]);
}

#[test]
fn kitty_escapes_and_a_remapped_ctrl_c_cancel_the_question() {
    for sequence in [&b"\x1b[27u"[..], b"\x1b[27;1u", b"\x1b[99;5u"] {
        let mut test = asking(proceed());
        press(&mut test, sequence);
        assert_eq!(answers(&test), [answered(4, None)], "{sequence:?}");
        assert!(test.sent().contains(&UiCommand::Cancel {
            turn_id: TurnId::new(1)
        }));
    }
}

#[test]
fn a_question_hides_an_open_skills_menu_until_it_is_answered() {
    let mut test = TestShell::start();
    test.submit("pick for me");
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(1),
    });
    test.deliver(UiEvent::SkillsMenu {
        items: vec![SkillMenuItem {
            name: "review".to_owned(),
            description: "review workflow".to_owned(),
            path: PathBuf::from("/skills/review"),
            source: SkillMenuSource::OhFx,
            group: SkillMenuGroup::Workspace,
            scope: "oh-fx · Workspace".to_owned(),
            source_label: String::new(),
        }],
        focus: SkillMenuFocus::Start,
    });
    let screen = test.screen();
    assert!(screen.contains("Skills 1"), "{screen}");
    test.deliver(requested(1, 4, proceed()));
    let screen = test.screen();
    assert!(screen.contains(HINT), "{screen}");
    assert!(!screen.contains("Skills 1"), "{screen}");
    assert!(!screen.contains("enter use"), "{screen}");
    press(&mut test, b"2");
    assert_eq!(answers(&test), [answered(4, Some(&["No"]))]);
    let screen = test.screen();
    assert!(screen.contains("Skills 1"), "{screen}");
    assert!(screen.contains("enter use"), "{screen}");
}

#[test]
fn escaped_controls_in_an_answer_stay_visible_in_the_editor_and_the_transcript() {
    let mut test = asking(proceed());
    test.resize(24, 24);
    press(&mut test, b"4");
    press(
        &mut test,
        "\x1b[200~\u{202e}\u{202e}\u{202e}END\x1b[201~".as_bytes(),
    );
    let screen = test.screen();
    assert!(
        screen.contains("    4) \\u{202e}\\u{202e}\n       \\u{202e}END"),
        "{screen}"
    );
    press(&mut test, b"\x1b[A");
    press(&mut test, b"!");
    press(&mut test, b"\r");
    assert_eq!(
        answers(&test),
        [answered(4, Some(&["\u{202e}!\u{202e}\u{202e}END"]))]
    );
    let screen = test.screen();
    assert!(
        screen.contains("  1) Should we proceed?\n     \\u{202e}!\\u{202e}\n     \\u{202e}END"),
        "{screen}"
    );
}
