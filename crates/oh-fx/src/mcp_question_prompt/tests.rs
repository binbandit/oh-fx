use super::*;

fn entry(question: &str, labels: &[(&str, Option<&str>)]) -> QuestionBatchEntry {
    QuestionBatchEntry {
        question: question.to_owned(),
        options: labels
            .iter()
            .map(|(label, description)| QuestionOption {
                label: (*label).to_owned(),
                description: description.map(str::to_owned),
            })
            .collect(),
        submission: ofx_contract::QuestionSubmission::Label,
    }
}

#[test]
fn questions_are_written_as_numbered_choices_on_stderr() {
    let mut output = Vec::new();
    write_prompt(
        &entry(
            "Pick one",
            &[("Keep current", None), ("Edit value", Some("change it"))],
        ),
        &mut output,
        true,
    )
    .unwrap();
    write_prompt(&entry("Name?", &[]), &mut output, false).unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "\nPick one\n  1. Keep current\n  2. Edit value \u{2014} change it\nChoose a number or enter a value: \x07\nName?\nEnter a value: "
    );
}

#[test]
fn answers_map_ordinals_and_labels_and_keep_other_text() {
    let options = entry("q", &[("True", None), ("False", None)]).options;
    for (line, expected) in [
        ("1", "True"),
        ("+2", "False"),
        (" False\r", "False"),
        ("3", "3"),
        ("0", "0"),
        ("-1", "-1"),
        ("1_0", "1_0"),
        ("maybe", "maybe"),
        ("", ""),
    ] {
        assert_eq!(answer(&options, line), expected, "{line}");
    }
}

#[test]
fn end_of_input_and_long_lines_cancel_and_other_bytes_are_kept() {
    let options = entry("q", &[("True", None)]).options;
    assert_eq!(answer_line(&options, &Line::Closed), None);
    assert_eq!(answer_line(&options, &Line::TooLong), None);
    assert_eq!(
        answer_line(&options, &Line::Text(b" 1 ".to_vec())),
        Some("True".to_owned())
    );
    assert_eq!(
        answer_line(&options, &Line::Text(b"a\xffb".to_vec())),
        Some("a\u{fffd}b".to_owned())
    );
}
