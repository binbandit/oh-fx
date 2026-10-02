use ofx_contract::{QuestionBatchEntry, QuestionOption, QuestionRequest, RequestId};

use super::*;
use crate::row_text::Attribute;
use crate::shell::question_prompt::QuestionPrompt;

fn theme() -> Theme {
    Theme::builtin(false, false, true)
}

fn option(label: &str, description: Option<&str>) -> QuestionOption {
    QuestionOption {
        label: label.to_owned(),
        description: description.map(str::to_owned),
    }
}

fn prompt(entries: &[(&str, Vec<QuestionOption>)]) -> QuestionPrompt {
    QuestionPrompt::new(QuestionRequest {
        id: RequestId::new(1),
        entries: entries
            .iter()
            .map(|(question, options)| QuestionBatchEntry {
                question: (*question).to_owned(),
                options: options.clone(),
            })
            .collect(),
    })
}

fn proceed() -> QuestionPrompt {
    prompt(&[(
        "Should we proceed?",
        vec![
            option("Yes", Some("go ahead")),
            option("No", None),
            option("Maybe", Some("decide later")),
        ],
    )])
}

fn panel(prompt: &QuestionPrompt, cols: usize) -> Vec<Row> {
    question_panel_rows(&theme(), prompt.view().entry.unwrap(), cols)
}

fn texts(rows: &[Row]) -> Vec<String> {
    rows.iter().map(Row::text).collect()
}

fn indent_before(rows: &[Row], needle: &str) -> usize {
    let text = texts(rows)
        .into_iter()
        .find(|text| text.contains(needle))
        .unwrap_or_else(|| panic!("{needle} in {:?}", texts(rows)));
    visible_width(&text[..text.find(needle).unwrap()])
}

fn assert_fits(rows: &[Row], cols: usize) {
    for row in rows {
        assert!(row.width() <= cols, "{:?}", row.text());
    }
}

fn has_reverse(rows: &[Row]) -> bool {
    rows.iter().any(|row| {
        row.segments()
            .iter()
            .any(|segment| segment.paint.has(Attribute::Reverse))
    })
}

#[test]
fn the_panel_shows_the_question_a_gap_and_the_choices_without_a_hint() {
    let rows = panel(&proceed(), 80);
    let texts = texts(&rows);
    assert_eq!(texts[0], "  Should we proceed?");
    assert_eq!(texts[1], "");
    assert!(texts[2].starts_with("    1) Yes"), "{texts:?}");
    assert!(texts[2].contains("go ahead"), "{texts:?}");
    assert_eq!(texts[3], "    2) No");
    assert!(texts[5].starts_with("    4) Other"), "{texts:?}");
    assert_eq!(texts.len(), 6);
    assert!(rows[0].segments()[1].paint.has(Attribute::Bold));
    assert!(!texts.iter().any(|text| text.contains("esc cancel")));
}

#[test]
fn long_question_text_wraps_under_its_indent_without_an_ellipsis() {
    let prompt = prompt(&[(
        "Which verification steps should run before this branch is pushed?",
        vec![option("Run tests", None), option("Skip tests", None)],
    )]);
    let rows = panel(&prompt, 42);
    assert!(texts(&rows)[0].contains("Which verification steps should run"));
    assert_eq!(indent_before(&rows, "before this branch is pushed?"), 2);
    assert!(!texts(&rows).iter().any(|text| text.contains('…')));
    assert_fits(&rows, 42);
}

#[test]
fn a_full_batch_of_long_choices_stays_within_the_width() {
    let options = vec![
        option(
            "Inspect repository state",
            Some("Look around the workspace and report back"),
        ),
        option("Run unit tests", Some("Execute zig build test")),
        option("Build binary", Some("Execute zig build")),
        option("Review branch", Some("Check the local diff")),
        option("Ship branch", Some("Push and open a PR")),
        option("Something else", Some("Use freeform input")),
    ];
    let prompt = prompt(&[
        ("What should happen first?", options.clone()),
        ("What should happen second?", options.clone()),
        ("What should happen third?", options.clone()),
        ("What should happen fourth?", options),
    ]);
    for cols in [80, 60, 40, 12] {
        assert_fits(&panel(&prompt, cols), cols);
    }
}

#[test]
fn only_the_current_page_is_drawn_and_selection_is_shown_by_style_alone() {
    let mut prompt = prompt(&[
        ("Q0?", vec![option("One", None), option("Two", None)]),
        ("Q1?", vec![option("One", None), option("Two", None)]),
        ("Q2?", vec![option("One", None), option("Two", None)]),
    ]);
    prompt.submit();
    let rows = panel(&prompt, 60);
    let joined = texts(&rows).join("\n");
    assert!(joined.contains("Q1?"));
    assert!(!joined.contains("Q0?") && !joined.contains("Q2?"));
    assert!(!joined.contains("Question 2 of 3"));
    assert!(!joined.contains('❯'));
    assert_eq!(rows[2].segments()[0].text, "    1) One");
    assert_eq!(rows[2].segments()[0].paint, theme().selected_completion);
    assert_eq!(rows[3].segments()[0].text, "    2) Two");
    assert_eq!(rows[3].segments()[0].paint, theme().dim);
}

#[test]
fn the_freeform_slot_shows_its_placeholder_until_selected_and_then_a_cursor() {
    let mut prompt = proceed();
    let rows = panel(&prompt, 60);
    let other = rows
        .iter()
        .find(|row| row.text().contains("Other"))
        .unwrap();
    assert_eq!(other.segments()[0].paint, theme().dim);
    assert!(!has_reverse(&rows));
    prompt.move_choice(-1);
    let rows = panel(&prompt, 60);
    assert!(!texts(&rows).iter().any(|text| text.contains("Other")));
    assert!(has_reverse(&rows));
    prompt.insert("hi", usize::MAX);
    let rows = panel(&prompt, 60);
    assert!(texts(&rows).iter().any(|text| text == "    4) hi "));
    assert!(has_reverse(&rows));
    prompt.move_choice(1);
    let rows = panel(&prompt, 60);
    assert!(texts(&rows).iter().any(|text| text == "    4) hi"));
}

#[test]
fn a_long_freeform_answer_wraps_without_an_ellipsis() {
    let mut prompt = prompt(&[(
        "Should we proceed?",
        vec![option("Yes", Some("go ahead")), option("No", None)],
    )]);
    prompt.move_choice(-1);
    prompt.insert("abcdefghijklmnopqrstuvwxyz0123456789", usize::MAX);
    let rows = panel(&prompt, 24);
    let texts = texts(&rows);
    for piece in ["abcdefghijklmnopq", "rstuvwxyz01234567", "89"] {
        assert!(texts.iter().any(|text| text.contains(piece)), "{texts:?}");
    }
    assert!(!texts.iter().any(|text| text.contains('…')));
    assert_fits(&rows, 24);
    assert_eq!(rows.len(), 8);
}

#[test]
fn a_cursor_at_an_exact_wrap_boundary_gets_a_row_of_its_own() {
    let mut prompt = prompt(&[(
        "Should we proceed?",
        vec![option("Yes", None), option("No", None)],
    )]);
    prompt.move_choice(-1);
    prompt.insert("abcdefghijklmnopq", usize::MAX);
    let rows = panel(&prompt, 24);
    let last = rows.last().unwrap();
    assert_eq!(last.text(), "        ");
    assert!(last.segments()[1].paint.has(Attribute::Reverse));
    assert_eq!(rows.len(), 6);
}

#[test]
fn pasted_line_breaks_become_rows() {
    let mut prompt = prompt(&[(
        "Should we proceed?",
        vec![option("Yes", None), option("No", None)],
    )]);
    prompt.move_choice(-1);
    prompt.insert("first line\nsecond line", usize::MAX);
    let rows = panel(&prompt, 24);
    let texts = texts(&rows);
    assert_eq!(texts[4], "    3) first line");
    assert_eq!(texts[5], "       second line ");
    assert_eq!(rows.len(), 6);
}

#[test]
fn long_labels_and_descriptions_wrap_in_their_own_columns() {
    let prompt_with = |description: Option<&str>| {
        prompt(&[(
            "Which action should I take?",
            vec![
                option(
                    if description.is_some() {
                        "Thorough"
                    } else {
                        "Run the complete verification suite before pushing"
                    },
                    description,
                ),
                option("Skip", None),
            ],
        )])
    };
    let rows = panel(&prompt_with(None), 32);
    assert!(
        texts(&rows)
            .iter()
            .any(|text| text == "    1) Run the complete")
    );
    assert_eq!(indent_before(&rows, "verification suite"), 7);
    assert_eq!(indent_before(&rows, "before pushing"), 7);
    assert_fits(&rows, 32);
    let rows = panel(
        &prompt_with(Some(
            "Run the complete verification suite before pushing this branch",
        )),
        60,
    );
    assert_eq!(indent_before(&rows, "Run the complete verification"), 29);
    assert_eq!(indent_before(&rows, "suite before pushing this"), 29);
    assert_eq!(indent_before(&rows, "branch"), 29);
    assert!(texts(&rows)[2].starts_with("    1) Thorough"));
    let rows = panel(
        &prompt_with(Some("Run the complete verification suite")),
        40,
    );
    assert_eq!(texts(&rows)[3], "       Run the complete verification");
}

#[test]
fn the_hint_row_fits_the_width_and_names_the_choices() {
    let mut prompt = proceed();
    for (cols, hint) in [
        (
            120,
            "1–4 choose now    ↑↓ options    tab questions    enter answer    esc cancel",
        ),
        (
            88,
            "1–4 choose now    ↑↓ options    tab questions    enter answer    esc cancel",
        ),
        (40, "enter answer · esc cancel"),
    ] {
        let row = question_hint_row(&theme(), &prompt.view(), cols);
        assert_eq!(row.text(), hint);
        assert_eq!(row.segments()[0].paint, theme().dim);
    }
    prompt.move_choice(-1);
    for (cols, hint) in [
        (
            120,
            "type answer    ↑↓←→ cursor    shift+↑↓ options    tab questions    enter answer    esc cancel",
        ),
        (
            72,
            "↑↓ cursor · shift+↑↓ options · tab questions · enter answer · esc cancel",
        ),
        (32, "enter answer · esc cancel"),
    ] {
        assert_eq!(
            question_hint_row(&theme(), &prompt.view(), cols).text(),
            hint
        );
    }
}

#[test]
fn the_hint_row_right_aligns_batch_progress_and_drops_it_when_narrow() {
    let prompt = prompt(&[
        ("First?", vec![option("Yes", None), option("No", None)]),
        ("Second?", vec![option("Yes", None), option("No", None)]),
        ("Third?", vec![option("Yes", None), option("No", None)]),
    ]);
    let row = question_hint_row(&theme(), &prompt.view(), 120);
    assert!(row.text().ends_with("  Question 1 of 3"));
    assert_eq!(row.width(), 120);
    let narrow = question_hint_row(&theme(), &prompt.view(), 40);
    assert!(!narrow.text().contains("Question 1 of 3"));
}

#[test]
fn answered_questions_resolve_into_numbered_rows_with_hanging_answers() {
    let answers = [
        ("Q0?".to_owned(), "Alpha".to_owned()),
        ("Q1?".to_owned(), "Beta".to_owned()),
    ];
    let rows = resolution_rows(&theme(), &answers, 60);
    assert_eq!(
        texts(&rows),
        ["  1) Q0?", "     Alpha", "  2) Q1?", "     Beta"]
    );
    assert_eq!(rows[0].segments()[0].paint, Paint::PLAIN);
    assert_eq!(rows[1].segments()[0].paint, theme().statusline);
    let cancelled = cancelled_resolution_row(&theme());
    assert_eq!(cancelled.text(), "■ Cancelled");
    assert_eq!(cancelled.segments()[0].paint, theme().red);
    assert_eq!(cancelled.segments()[1].paint, Paint::PLAIN);
}

#[test]
fn multiline_resolution_fields_keep_every_row_inside_the_rail() {
    let answers = [(
        "Choose one\ncarefully".to_owned(),
        "line-one\nline-two\nline-three".to_owned(),
    )];
    let rows = resolution_rows(&theme(), &answers, 60);
    assert_eq!(
        texts(&rows),
        [
            "  1) Choose one",
            "     carefully",
            "     line-one",
            "     line-two",
            "     line-three",
        ]
    );
    let empty = [("Q?".to_owned(), String::new())];
    assert_eq!(
        texts(&resolution_rows(&theme(), &empty, 60)),
        ["  1) Q?", "     "]
    );
}

#[test]
fn long_resolutions_wrap_into_hanging_rows_at_every_width() {
    let answers = [(
        "Should the background summary cover only the assistant's written reply, or its entire completed turn including tool calls and results?".to_owned(),
        "Cover the entire completed turn including tool calls and results while keeping the latest user context visible".to_owned(),
    )];
    let rows = resolution_rows(&theme(), &answers, 40);
    let joined = texts(&rows).join("\n");
    assert!(!joined.contains('…'));
    assert_eq!(
        indent_before(&rows, "cover only the assistant's written"),
        5
    );
    assert_eq!(indent_before(&rows, "results?"), 5);
    assert_eq!(indent_before(&rows, "context visible"), 5);
    assert_fits(&rows, 40);
    for cols in [12, 5, 1] {
        assert_fits(&resolution_rows(&theme(), &answers, cols), cols);
    }
}
