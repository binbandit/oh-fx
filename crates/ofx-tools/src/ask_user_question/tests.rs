use std::future::{pending, ready};
use std::sync::Mutex;

use ofx_contract::{PathAccess, ToolCallId, ToolResultStatus};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::*;

enum Reply {
    FirstLabels,
    Answers(Vec<String>),
    Cancel,
    Hold,
}

struct FakeAsker {
    asked: Mutex<Vec<Vec<QuestionBatchEntry>>>,
    reply: Reply,
}

impl FakeAsker {
    fn replying(reply: Reply) -> Arc<Self> {
        Arc::new(Self {
            asked: Mutex::new(Vec::new()),
            reply,
        })
    }

    fn asked(&self) -> Vec<Vec<QuestionBatchEntry>> {
        self.asked.lock().unwrap().clone()
    }
}

impl QuestionAsker for FakeAsker {
    fn ask(&self, entries: Vec<QuestionBatchEntry>) -> BoxFuture<'static, Option<Vec<String>>> {
        let answers = match &self.reply {
            Reply::FirstLabels => Some(
                entries
                    .iter()
                    .map(|entry| entry.options[0].label.clone())
                    .collect(),
            ),
            Reply::Answers(answers) => Some(answers.clone()),
            Reply::Cancel | Reply::Hold => None,
        };
        self.asked.lock().unwrap().push(entries);
        if matches!(self.reply, Reply::Hold) {
            return Box::pin(pending());
        }
        Box::pin(ready(answers))
    }
}

fn tool(asker: &Arc<FakeAsker>) -> AskUserQuestion {
    AskUserQuestion::new(Some(Arc::clone(asker) as Arc<dyn QuestionAsker>))
}

fn run(tool: &AskUserQuestion, arguments: &str) -> ToolOutput {
    run_with(tool, arguments, CancellationToken::new())
}

fn run_with(
    tool: &AskUserQuestion,
    arguments: &str,
    cancellation: CancellationToken,
) -> ToolOutput {
    let prepared = tool.prepare(arguments).unwrap();
    if let Some(refusal) = prepared.refusal() {
        return refusal.clone();
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let context = ToolContext::new(
        ToolCallId::new("call-1"),
        cancellation,
        PathAccess::WorkspaceOnly,
    );
    runtime.block_on(prepared.execute(context))
}

fn option(label: &str, description: Option<&str>) -> QuestionOption {
    QuestionOption {
        label: label.to_owned(),
        description: description.map(str::to_owned),
    }
}

#[test]
fn the_schema_and_description_match_upstream_byte_for_byte() {
    let tool = AskUserQuestion::new(None);
    let spec = tool.spec();
    assert_eq!(spec.name, "ask_user_question");
    assert_eq!(spec.description, DESCRIPTION);
    for fragment in [
        "only when a concrete decision blocks progress",
        "after local files, git state, or tool output cannot answer it",
        "precise, mutually exclusive paths",
        "GitHub handles unless account/private-access specific",
        "gh/auth/tool blockers",
        "noninteractive runs should surface a blocker in freeform text",
    ] {
        assert!(spec.description.contains(fragment), "{fragment}");
    }
    for fragment in [
        r#""questions":{"type":"array","minItems":1,"maxItems":4"#,
        r#""options":{"type":"array","minItems":2,"maxItems":6"#,
        "Specific blocking decision shown to the user; do not ask for facts tools can inspect.",
        "Short precise action label, 1-5 words.",
        "Optional one-line consequence or scope of this option.",
        r#""required":["question","options"]"#,
        r#""required":["questions"]}"#,
    ] {
        assert!(spec.input_schema.contains(fragment), "{fragment}");
    }
    let schema: Value = serde_json::from_str(&spec.input_schema).unwrap();
    assert_eq!(schema.to_string(), spec.input_schema);
}

#[test]
fn calls_are_serial_read_only_asks_titled_like_upstream() {
    let tool = AskUserQuestion::new(None);
    let description = tool.prepare("{}").unwrap().describe();
    assert_eq!(description.title, "Asking ");
    assert_eq!(description.activity, ToolActivity::Ask);
    assert_eq!(description.effect, ToolEffect::ReadOnly);
    assert_eq!(description.concurrency, Concurrency::Serial);
    assert_eq!(
        tool.prepare("not-json").unwrap().describe().title,
        "Working: ask_user_question"
    );
}

#[test]
fn arguments_are_validated_in_upstream_order_with_its_messages() {
    let asker = FakeAsker::replying(Reply::FirstLabels);
    let tool = tool(&asker);
    let cases = [
        (
            "not-json",
            "(ask_user_question: invalid arguments; provide {questions})",
        ),
        (
            "[]",
            "(ask_user_question: invalid arguments; provide {questions})",
        ),
        (
            r#"{"questions":[],"questions":[]}"#,
            "(ask_user_question: invalid arguments; provide {questions})",
        ),
        (
            "{}",
            "(ask_user_question: missing required array \"questions\")",
        ),
        (
            r#"{"questions":{}}"#,
            "(ask_user_question: \"questions\" must be an array)",
        ),
        (
            r#"{"questions":[]}"#,
            "(ask_user_question: provide 1 to 4 questions)",
        ),
        (
            r#"{"questions":[1]}"#,
            "(ask_user_question: each question must be an object with a \"question\" and \"options\")",
        ),
        (
            r#"{"questions":[{}]}"#,
            "(ask_user_question: each question requires a \"question\" string)",
        ),
        (
            r#"{"questions":[{"question":1}]}"#,
            "(ask_user_question: question \"question\" must be a string)",
        ),
        (
            r#"{"questions":[{"question":"  ","options":[]}]}"#,
            "(ask_user_question: question text must not be empty)",
        ),
        (
            r#"{"questions":[{"question":"Q?"}]}"#,
            "(ask_user_question: each question requires an \"options\" array)",
        ),
        (
            r#"{"questions":[{"question":"Q?","options":1}]}"#,
            "(ask_user_question: \"options\" must be an array)",
        ),
        (
            r#"{"questions":[{"question":"Q?","options":[]}]}"#,
            "(ask_user_question: provide 2 to 6 options per question)",
        ),
        (
            r#"{"questions":[{"question":"Q?","options":[1,2]}]}"#,
            "(ask_user_question: each option must be an object with a \"label\")",
        ),
        (
            r#"{"questions":[{"question":"Q?","options":[{},{}]}]}"#,
            "(ask_user_question: each option requires a \"label\" string)",
        ),
        (
            r#"{"questions":[{"question":"Q?","options":[{"label":1},{"label":"No"}]}]}"#,
            "(ask_user_question: option \"label\" must be a string)",
        ),
        (
            r#"{"questions":[{"question":"Q?","options":[{"label":""},{"label":"No"}]}]}"#,
            "(ask_user_question: option labels must not be empty)",
        ),
        (
            r#"{"questions":[{"question":"Q?","options":[{"label":"Yes"},{"label":" yes "}]}]}"#,
            "(ask_user_question: option labels must be unique within a question)",
        ),
    ];
    for (arguments, expected) in cases {
        let output = run(&tool, arguments);
        assert_eq!(output, ToolOutput::success(expected), "{arguments}");
    }
    let five = format!(
        r#"{{"questions":[{}]}}"#,
        [r#"{"question":"Q?","options":[{"label":"a"},{"label":"b"}]}"#; 5].join(",")
    );
    assert_eq!(
        run(&tool, &five).content,
        "(ask_user_question: provide 1 to 4 questions)"
    );
    let seven = format!(
        r#"{{"questions":[{{"question":"Q?","options":[{}]}}]}}"#,
        (0..7)
            .map(|index| format!(r#"{{"label":"{index}"}}"#))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert_eq!(
        run(&tool, &seven).content,
        "(ask_user_question: provide 2 to 6 options per question)"
    );
    assert!(asker.asked().is_empty());
}

#[test]
fn trimmed_questions_reach_the_user_and_answers_return_in_order() {
    let asker = FakeAsker::replying(Reply::FirstLabels);
    let output = run(
        &tool(&asker),
        r#"{"questions":[{"question":" Choose? ","options":[{"label":" Yes ","description":" Go ahead "},{"label":"No","description":3},{"label":"Later","description":"  "}]}]}"#,
    );
    assert_eq!(
        asker.asked(),
        [vec![QuestionBatchEntry {
            question: "Choose?".to_owned(),
            options: vec![
                option("Yes", Some("Go ahead")),
                option("No", None),
                option("Later", None),
            ],
        }]]
    );
    assert_eq!(
        output,
        ToolOutput::success(r#"[{"question":"Choose?","answer":"Yes"}]"#)
    );
    let asker = FakeAsker::replying(Reply::Answers(vec![
        "Thorough".to_owned(),
        "Yes\nnow".to_owned(),
    ]));
    let output = run(
        &tool(&asker),
        r#"{"questions":[{"question":"Which depth?","options":[{"label":"Thorough"},{"label":"Fast"}]},{"question":"Ship it?","options":[{"label":"Yes"},{"label":"No"}]}]}"#,
    );
    assert_eq!(
        output.content,
        r#"[{"question":"Which depth?","answer":"Thorough"},{"question":"Ship it?","answer":"Yes\nnow"}]"#
    );
    let asker = FakeAsker::replying(Reply::Answers(vec![
        "\"\\\u{1}\u{8}\u{c}\t\r\u{7f}/é".to_owned(),
    ]));
    let output = run(
        &tool(&asker),
        r#"{"questions":[{"question":"Say \"why\"?","options":[{"label":"A"},{"label":"B"}]}]}"#,
    );
    assert_eq!(
        output.content,
        "[{\"question\":\"Say \\\"why\\\"?\",\"answer\":\"\\\"\\\\\\u0001\\b\\f\\t\\r\u{7f}/é\"}]"
    );
}

#[test]
fn model_supplied_text_is_flattened_and_made_terminal_safe_before_it_is_shown() {
    let asker = FakeAsker::replying(Reply::FirstLabels);
    let output = run(
        &tool(&asker),
        r#"{"questions":[{"question":"Q\n\u001b[31m?","options":[{"label":"Alpha\nFake","description":"Desc\tGap"},{"label":"\u001b[31mRed\u001b[0m"},{"label":"a  b"},{"label":"\u202eevil \u000b\u000c x"}]}]}"#,
    );
    assert_eq!(
        asker.asked()[0],
        [QuestionBatchEntry {
            question: "Q \\x1b[31m?".to_owned(),
            options: vec![
                option("Alpha Fake", Some("Desc Gap")),
                option("\\x1b[31mRed\\x1b[0m", None),
                option("a  b", None),
                option("\\u{202e}evil x", None),
            ],
        }]
    );
    assert_eq!(
        output.content,
        r#"[{"question":"Q \\x1b[31m?","answer":"Alpha Fake"}]"#
    );
}

#[test]
fn labels_that_differ_only_in_ascii_case_after_encoding_are_duplicates() {
    let asker = FakeAsker::replying(Reply::FirstLabels);
    let output = run(
        &tool(&asker),
        r#"{"questions":[{"question":"Q?","options":[{"label":"Go\nNow"},{"label":"go now"}]}]}"#,
    );
    assert_eq!(
        output.content,
        "(ask_user_question: option labels must be unique within a question)"
    );
    let output = run(
        &tool(&asker),
        r#"{"questions":[{"question":"Q?","options":[{"label":"É"},{"label":"é"}]}]}"#,
    );
    assert_eq!(output.content, r#"[{"question":"Q?","answer":"É"}]"#);
}

#[test]
fn a_cancelled_question_returns_the_cancel_sentinel() {
    let asker = FakeAsker::replying(Reply::Cancel);
    let output = run(
        &tool(&asker),
        r#"{"questions":[{"question":"Q?","options":[{"label":"Yes"},{"label":"No"}]}]}"#,
    );
    assert_eq!(output, ToolOutput::success("(user cancelled the question)"));
}

#[test]
fn a_cancelled_turn_stops_waiting_for_the_answer() {
    let asker = FakeAsker::replying(Reply::Hold);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let output = run_with(
        &tool(&asker),
        r#"{"questions":[{"question":"Q?","options":[{"label":"Yes"},{"label":"No"}]}]}"#,
        cancellation,
    );
    assert_eq!(output, ToolOutput::success("(user cancelled the question)"));
    assert_eq!(asker.asked().len(), 1);
}

#[test]
fn an_answer_count_that_differs_from_the_questions_fails_the_call() {
    let asker = FakeAsker::replying(Reply::Answers(vec!["a".to_owned(), "b".to_owned()]));
    let output = run(
        &tool(&asker),
        r#"{"questions":[{"question":"Q?","options":[{"label":"Yes"},{"label":"No"}]}]}"#,
    );
    assert_eq!(
        output,
        ToolOutput::failure("ask_user_question failed: AnswerCountMismatch")
    );
}

#[test]
fn legacy_permission_references_are_refused_before_prompting() {
    for asker in [None, Some(FakeAsker::replying(Reply::FirstLabels))] {
        let tool = AskUserQuestion::new(
            asker
                .as_ref()
                .map(|asker| Arc::clone(asker) as Arc<dyn QuestionAsker>),
        );
        let prepared = tool
            .prepare(r#"{"permission_request_id":"legacy","questions":[]}"#)
            .unwrap();
        assert_eq!(prepared.describe().effect, ToolEffect::None);
        let refusal = prepared.refusal().unwrap();
        assert_eq!(refusal.status, ToolResultStatus::Failure);
        assert_eq!(
            refusal.content,
            "(ask_user_question: permission_request_id is no longer supported; use the safety review advice to choose a different action)"
        );
        assert!(asker.is_none_or(|asker| asker.asked().is_empty()));
    }
}

#[test]
fn noninteractive_runs_return_the_not_available_sentinel_before_parsing() {
    let tool = AskUserQuestion::new(None);
    for arguments in [
        "not-json",
        r#"{"questions":[{"question":"Q?","options":[{"label":"Yes"},{"label":"No"}]}]}"#,
    ] {
        assert_eq!(
            run(&tool, arguments),
            ToolOutput::success(
                "(ask_user_question is only available in the interactive shell; ask the user freeform instead)"
            ),
            "{arguments}"
        );
    }
}

#[test]
fn saved_answers_decode_only_from_answered_question_results() {
    let encoded = r#"[{"question":"Which depth?","answer":"Thorough"},{"question":"Ship it?","answer":"Yes\nnow"}]"#;
    let saved = |output: &str| {
        let output = output.to_owned();
        move || Some(output)
    };
    assert_eq!(
        answered_questions("ask_user_question", saved(encoded)),
        Some(vec![
            ("Which depth?".to_owned(), "Thorough".to_owned()),
            ("Ship it?".to_owned(), "Yes\nnow".to_owned()),
        ])
    );
    assert_eq!(
        answered_questions("shell", || unreachable!("only question results are read")),
        None
    );
    assert_eq!(answered_questions("ask_user_question", || None), None);
    let five = format!("[{}]", [r#"{"question":"Q","answer":"A"}"#; 5].join(","));
    for output in [
        "(user cancelled the question)",
        "[]",
        r#"[{"question":"Q"}]"#,
        r#"[{"question":"Q","answer":1}]"#,
        r#"[{"question":"Q","answer":"A"}] trailing"#,
        r#"{"question":"Q","answer":"A"}"#,
        five.as_str(),
    ] {
        assert_eq!(
            answered_questions("ask_user_question", saved(output)),
            None,
            "{output}"
        );
    }
}
