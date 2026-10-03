use std::future::ready;
use std::sync::Arc;

use ofx_contract::{
    ActionLabel, BoxFuture, CallDescription, CallPresentation, Concurrency, PreparedCall,
    QuestionAsker, QuestionBatchEntry, QuestionOption, Tool, ToolActivity, ToolContext, ToolEffect,
    ToolOutput, ToolSpec, format_unknown_action, parse_tool_args_object,
};
use ofx_text::encode_terminal_safe;
use serde_json::Value;

const TOOL_NAME: &str = "ask_user_question";
const DESCRIPTION: &str = "Ask the user 1-4 multiple-choice questions in interactive runs only when a concrete decision blocks progress after local files, git state, or tool output cannot answer it. When to use: choose among precise, mutually exclusive paths before acting, especially user-preference decisions. When NOT to use: safety-review escalation, discoverable facts, GitHub handles unless account/private-access specific, gh/auth/tool blockers, trivial yes/no checks, open-ended discussion, or noninteractive runs; noninteractive runs should surface a blocker in freeform text instead.";
const INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"questions":{"type":"array","minItems":1,"maxItems":4,"items":{"type":"object","properties":{"question":{"type":"string","description":"Specific blocking decision shown to the user; do not ask for facts tools can inspect."},"options":{"type":"array","minItems":2,"maxItems":6,"items":{"type":"object","properties":{"label":{"type":"string","description":"Short precise action label, 1-5 words."},"description":{"type":"string","description":"Optional one-line consequence or scope of this option."}},"required":["label"]}}},"required":["question","options"]}}},"required":["questions"]}"#;
const PRESENTATION: CallPresentation = CallPresentation {
    activity: ToolActivity::Ask,
    action_label: "Asking",
    completed_label: "Asked",
    label_argument: "",
    label_default: "",
};
const CANCEL_SENTINEL: &str = "(user cancelled the question)";
const NOT_AVAILABLE_SENTINEL: &str =
    "(ask_user_question is only available in the interactive shell; ask the user freeform instead)";
const LEGACY_PERMISSION_REQUEST_SENTINEL: &str = "(ask_user_question: permission_request_id is no longer supported; use the safety review advice to choose a different action)";
const ANSWER_COUNT_MISMATCH: &str = "ask_user_question failed: AnswerCountMismatch";
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];
const MAX_QUESTIONS: usize = 4;
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 6;

pub struct AskUserQuestion {
    spec: ToolSpec,
    asker: Option<Arc<dyn QuestionAsker>>,
}

impl AskUserQuestion {
    pub fn new(asker: Option<Arc<dyn QuestionAsker>>) -> Self {
        Self {
            spec: ToolSpec {
                name: TOOL_NAME.to_owned(),
                description: DESCRIPTION.to_owned(),
                input_schema: INPUT_SCHEMA,
            },
            asker,
        }
    }
}

impl Tool for AskUserQuestion {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        let parsed = parse_tool_args_object(arguments);
        let refusal = parsed
            .as_ref()
            .is_ok_and(|arguments| arguments.get("permission_request_id").is_some())
            .then(|| ToolOutput::failure(LEGACY_PERMISSION_REQUEST_SENTINEL));
        let label = parsed.is_ok().then(|| PRESENTATION.label(""));
        let description = CallDescription {
            title: label
                .as_ref()
                .map_or_else(|| format_unknown_action(TOOL_NAME), ActionLabel::title),
            label,
            activity: PRESENTATION.activity,
            effect: if refusal.is_some() {
                ToolEffect::None
            } else {
                ToolEffect::ReadOnly
            },
            concurrency: Concurrency::Serial,
        };
        Ok(Box::new(AskCall {
            description,
            refusal,
            arguments: arguments.to_owned(),
            asker: self.asker.clone(),
        }))
    }

    fn describe_saved(&self, arguments: &str) -> Option<CallDescription> {
        self.prepare(arguments).ok().map(|call| call.describe())
    }
}

struct AskCall {
    description: CallDescription,
    refusal: Option<ToolOutput>,
    arguments: String,
    asker: Option<Arc<dyn QuestionAsker>>,
}

impl PreparedCall for AskCall {
    fn describe(&self) -> CallDescription {
        self.description.clone()
    }

    fn refusal(&self) -> Option<&ToolOutput> {
        self.refusal.as_ref()
    }

    fn execute(self: Box<Self>, context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        let AskCall {
            refusal,
            arguments,
            asker,
            ..
        } = *self;
        if let Some(refusal) = refusal {
            return Box::pin(ready(refusal));
        }
        let Some(asker) = asker else {
            return Box::pin(ready(ToolOutput::success(NOT_AVAILABLE_SENTINEL)));
        };
        let entries = match parse_question_batch(&arguments) {
            Ok(entries) => entries,
            Err(error) => return Box::pin(ready(ToolOutput::success(error.body()))),
        };
        let request = asker.ask(entries.clone());
        Box::pin(async move {
            match context
                .cancellation
                .run_until_cancelled_owned(request)
                .await
                .flatten()
            {
                Some(answers) => encode_answers(&entries, &answers).map_or_else(
                    || ToolOutput::failure(ANSWER_COUNT_MISMATCH),
                    ToolOutput::success,
                ),
                None => ToolOutput::success(CANCEL_SENTINEL),
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchError {
    InvalidArguments,
    MissingQuestions,
    QuestionsNotArray,
    QuestionCount,
    QuestionNotObject,
    MissingQuestionText,
    QuestionTextNotString,
    EmptyQuestionText,
    MissingOptions,
    OptionsNotArray,
    OptionCount,
    OptionNotObject,
    MissingOptionLabel,
    OptionLabelNotString,
    EmptyOptionLabel,
    DuplicateOptionLabel,
}

impl BatchError {
    fn body(self) -> &'static str {
        match self {
            Self::InvalidArguments => "(ask_user_question: invalid arguments; provide {questions})",
            Self::MissingQuestions => "(ask_user_question: missing required array \"questions\")",
            Self::QuestionsNotArray => "(ask_user_question: \"questions\" must be an array)",
            Self::QuestionCount => "(ask_user_question: provide 1 to 4 questions)",
            Self::QuestionNotObject => {
                "(ask_user_question: each question must be an object with a \"question\" and \"options\")"
            }
            Self::MissingQuestionText => {
                "(ask_user_question: each question requires a \"question\" string)"
            }
            Self::QuestionTextNotString => {
                "(ask_user_question: question \"question\" must be a string)"
            }
            Self::EmptyQuestionText => "(ask_user_question: question text must not be empty)",
            Self::MissingOptions => {
                "(ask_user_question: each question requires an \"options\" array)"
            }
            Self::OptionsNotArray => "(ask_user_question: \"options\" must be an array)",
            Self::OptionCount => "(ask_user_question: provide 2 to 6 options per question)",
            Self::OptionNotObject => {
                "(ask_user_question: each option must be an object with a \"label\")"
            }
            Self::MissingOptionLabel => {
                "(ask_user_question: each option requires a \"label\" string)"
            }
            Self::OptionLabelNotString => "(ask_user_question: option \"label\" must be a string)",
            Self::EmptyOptionLabel => "(ask_user_question: option labels must not be empty)",
            Self::DuplicateOptionLabel => {
                "(ask_user_question: option labels must be unique within a question)"
            }
        }
    }
}

fn parse_question_batch(arguments: &str) -> Result<Vec<QuestionBatchEntry>, BatchError> {
    if parse_tool_args_object(arguments).is_err() {
        return Err(BatchError::InvalidArguments);
    }
    let Ok(Value::Object(arguments)) = serde_json::from_str::<Value>(arguments) else {
        return Err(BatchError::InvalidArguments);
    };
    let questions = match arguments.get("questions") {
        None => return Err(BatchError::MissingQuestions),
        Some(Value::Array(questions)) => questions,
        Some(_) => return Err(BatchError::QuestionsNotArray),
    };
    if questions.is_empty() || questions.len() > MAX_QUESTIONS {
        return Err(BatchError::QuestionCount);
    }
    questions.iter().map(parse_entry).collect()
}

fn parse_entry(item: &Value) -> Result<QuestionBatchEntry, BatchError> {
    let Value::Object(item) = item else {
        return Err(BatchError::QuestionNotObject);
    };
    let question = match item.get("question") {
        None => return Err(BatchError::MissingQuestionText),
        Some(Value::String(question)) => question.trim_matches(TRIMMED),
        Some(_) => return Err(BatchError::QuestionTextNotString),
    };
    if question.is_empty() {
        return Err(BatchError::EmptyQuestionText);
    }
    let question = terminal_safe_question_text(question);
    let options = match item.get("options") {
        None => return Err(BatchError::MissingOptions),
        Some(Value::Array(options)) => options,
        Some(_) => return Err(BatchError::OptionsNotArray),
    };
    if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&options.len()) {
        return Err(BatchError::OptionCount);
    }
    let mut parsed: Vec<QuestionOption> = Vec::with_capacity(options.len());
    for option in options {
        let option = parse_option(option)?;
        if parsed
            .iter()
            .any(|existing| existing.label.eq_ignore_ascii_case(&option.label))
        {
            return Err(BatchError::DuplicateOptionLabel);
        }
        parsed.push(option);
    }
    Ok(QuestionBatchEntry {
        question,
        options: parsed,
    })
}

fn parse_option(option: &Value) -> Result<QuestionOption, BatchError> {
    let Value::Object(option) = option else {
        return Err(BatchError::OptionNotObject);
    };
    let label = match option.get("label") {
        None => return Err(BatchError::MissingOptionLabel),
        Some(Value::String(label)) => label.trim_matches(TRIMMED),
        Some(_) => return Err(BatchError::OptionLabelNotString),
    };
    if label.is_empty() {
        return Err(BatchError::EmptyOptionLabel);
    }
    let description = match option.get("description") {
        Some(Value::String(description)) => Some(description.trim_matches(TRIMMED))
            .filter(|description| !description.is_empty())
            .map(terminal_safe_question_text),
        _ => None,
    };
    Ok(QuestionOption {
        label: terminal_safe_question_text(label),
        description,
    })
}

fn terminal_safe_question_text(text: &str) -> String {
    let encoded = encode_terminal_safe(text.as_bytes(), usize::MAX).text;
    if encoded == text {
        return encoded;
    }
    let mut flattened = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if matches!(character, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}') {
            pending_space = !flattened.is_empty();
            continue;
        }
        if pending_space {
            flattened.push(' ');
            pending_space = false;
        }
        flattened.push(character);
    }
    encode_terminal_safe(flattened.as_bytes(), usize::MAX).text
}

fn encode_answers(entries: &[QuestionBatchEntry], answers: &[String]) -> Option<String> {
    if entries.len() != answers.len() {
        return None;
    }
    let mut encoded = String::from("[");
    for (index, (entry, answer)) in entries.iter().zip(answers).enumerate() {
        if index > 0 {
            encoded.push(',');
        }
        encoded.push_str("{\"question\":");
        encoded.push_str(&Value::String(entry.question.clone()).to_string());
        encoded.push_str(",\"answer\":");
        encoded.push_str(&Value::String(answer.clone()).to_string());
        encoded.push('}');
    }
    encoded.push(']');
    Some(encoded)
}

pub fn answered_questions(
    tool_name: &str,
    output: impl FnOnce() -> Option<String>,
) -> Option<Vec<(String, String)>> {
    if tool_name != TOOL_NAME {
        return None;
    }
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&output()?) else {
        return None;
    };
    if items.is_empty() || items.len() > MAX_QUESTIONS {
        return None;
    }
    items
        .iter()
        .map(|item| {
            let question = item.get("question")?.as_str()?;
            let answer = item.get("answer")?.as_str()?;
            Some((question.to_owned(), answer.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests;
