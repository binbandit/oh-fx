use std::fmt::{self, Write as _};
use std::sync::Arc;

use ofx_contract::{QuestionAsker, QuestionBatchEntry, QuestionOption, QuestionSubmission};
use ofx_text::encode_terminal_safe;
use serde_json::Value;
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::elicitation::{
    Answer, ElicitationCapabilities, ElicitationError, Field, FieldKind, FormSchema,
    HostClassification, Request, RequestKind, UrlRequest, classify_host, validate_answer,
    validate_field_json,
};
use crate::json_number::{Number, NumberError};
use crate::timing::timeout_at;

const MAX_FORM_ATTEMPTS: usize = 8;
const MAX_COMPACT_CHOICES: usize = 3;
const BROWSER_RETRIES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InteractionError {
    InvalidAnswer,
    McpInputTimedOut,
    PresentationLimitExceeded,
    RetryLimitExceeded,
    Elicitation(ElicitationError),
}

impl fmt::Display for InteractionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Elicitation(error) => write!(formatter, "{error:?}"),
            other => write!(formatter, "{other:?}"),
        }
    }
}

type Outcome<T> = Result<T, InteractionError>;

pub(crate) type UrlOpener = Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Clone)]
pub struct InputResponder {
    questioner: Arc<dyn QuestionAsker>,
    browser: UrlOpener,
    capabilities: ElicitationCapabilities,
    compact_forms: bool,
    turn: Arc<Mutex<()>>,
}

impl InputResponder {
    pub fn new(questioner: Arc<dyn QuestionAsker>, browser: UrlOpener) -> Self {
        Self {
            questioner,
            browser,
            capabilities: ElicitationCapabilities {
                form: true,
                url: true,
            },
            compact_forms: false,
            turn: Arc::default(),
        }
    }

    #[must_use]
    pub fn with_compact_forms(mut self) -> Self {
        self.compact_forms = true;
        self
    }

    pub(crate) fn capabilities(&self) -> ElicitationCapabilities {
        self.capabilities
    }

    pub(crate) async fn take_turn(
        &self,
        finished: &CancellationToken,
    ) -> Option<MutexGuard<'_, ()>> {
        let turn = tokio::select! {
            biased;
            () = finished.cancelled() => return None,
            turn = self.turn.lock() => turn,
        };
        (!finished.is_cancelled()).then_some(turn)
    }
}

impl fmt::Debug for InputResponder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputResponder")
            .field("capabilities", &self.capabilities)
            .field("compact_forms", &self.compact_forms)
            .finish_non_exhaustive()
    }
}

pub(crate) struct Interaction<'a> {
    pub(crate) responder: &'a InputResponder,
    pub(crate) server_name: &'a str,
    pub(crate) deadline: Instant,
    pub(crate) finished: &'a CancellationToken,
}

enum FieldAnswer {
    Cancelled,
    Declined,
    Value(Option<String>),
}

enum CompactAction {
    Value(Option<String>),
    Decline,
    Cancel,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FillKind {
    Scalar,
    SingleSelect,
    MultiSelect,
}

impl FillKind {
    fn action_label(self) -> &'static str {
        match self {
            Self::Scalar => "Enter value",
            Self::SingleSelect => "Select value",
            Self::MultiSelect => "Choose values",
        }
    }
}

enum FillAction {
    Enter,
    UseDefault,
    Skip,
    Cancelled,
}

enum Single {
    Cancelled,
    Invalid,
    Answer(String),
}

struct Current {
    answered: bool,
    json: Option<String>,
}

impl Interaction<'_> {
    pub(crate) async fn respond(&self, request: &Request) -> Outcome<Answer> {
        match &request.kind {
            RequestKind::Form(form) => self.answer_form(request, form).await,
            RequestKind::Url(url) => self.answer_url(url).await,
        }
    }

    async fn answer_form(&self, request: &Request, form: &FormSchema) -> Outcome<Answer> {
        let server = terminal_safe(self.server_name)?;
        let message = terminal_safe(&request.message)?;
        let compact = self.responder.compact_forms
            && form.fields.len() == 1
            && form.fields[0].kind == FieldKind::SingleSelect
            && form.fields[0].choices.len() <= MAX_COMPACT_CHOICES;
        let mut values: Vec<Current> = form
            .fields
            .iter()
            .map(|_| Current {
                answered: false,
                json: None,
            })
            .collect();
        for _ in 0..MAX_FORM_ATTEMPTS {
            let mut invalid = false;
            for (field, current) in form.fields.iter().zip(&mut values) {
                match self
                    .answer_field(&server, &message, field, current, compact)
                    .await
                {
                    Err(InteractionError::InvalidAnswer) => {
                        invalid = true;
                        break;
                    }
                    Err(error) => return Err(error),
                    Ok(FieldAnswer::Declined) => return Ok(Answer::Decline),
                    Ok(FieldAnswer::Cancelled) => return Ok(Answer::Cancel),
                    Ok(FieldAnswer::Value(json)) => {
                        current.json = json;
                        current.answered = true;
                    }
                }
            }
            if invalid || values.iter().any(|value| !value.answered) {
                if !self.retry_invalid_form(&server).await? {
                    return Ok(Answer::Cancel);
                }
                continue;
            }
            let response = Answer::AcceptForm(content(form, &values));
            if validate_answer(request, &response).is_err() {
                if !self.retry_invalid_form(&server).await? {
                    return Ok(Answer::Cancel);
                }
                continue;
            }
            if compact {
                return Ok(response);
            }
            let review = review_question(&server, &message, form, &values)?;
            let answer = match self
                .ask_single(
                    review,
                    labels(&["Submit", "Edit", "Decline", "Cancel"]),
                    QuestionSubmission::Label,
                )
                .await?
            {
                Single::Cancelled => return Ok(Answer::Cancel),
                Single::Invalid => continue,
                Single::Answer(answer) => answer,
            };
            match answer.as_str() {
                "Submit" => return Ok(response),
                "Decline" => return Ok(Answer::Decline),
                "Cancel" => return Ok(Answer::Cancel),
                _ => {}
            }
        }
        Err(InteractionError::RetryLimitExceeded)
    }

    async fn answer_field(
        &self,
        server: &str,
        message: &str,
        field: &Field,
        current: &Current,
        compact: bool,
    ) -> Outcome<FieldAnswer> {
        if compact && !current.answered {
            return self.answer_compact_field(server, message, field).await;
        }
        if current.answered {
            match self.keep_current_value(server, field, current).await? {
                Some(true) => return Ok(FieldAnswer::Value(current.json.clone())),
                Some(false) => {}
                None => return Ok(FieldAnswer::Cancelled),
            }
        }
        if field.kind == FieldKind::MultiSelect {
            return self.answer_multi_select(server, message, field).await;
        }
        let name = terminal_safe(field.display_name())?;
        let necessity = necessity(field);
        let question = match field
            .description
            .as_deref()
            .map(terminal_safe)
            .transpose()?
        {
            Some(description) => format!(
                "MCP server {server} requests {name}{necessity} \u{2014} {description}\nReason: {message}"
            ),
            None => format!("MCP server {server} requests {name}{necessity}\nReason: {message}"),
        };
        if field.default_json.is_some() || !field.required {
            let kind = if field.kind == FieldKind::SingleSelect {
                FillKind::SingleSelect
            } else {
                FillKind::Scalar
            };
            match self
                .fill_action(server, message, &name, field, kind)
                .await?
            {
                FillAction::Enter => {}
                FillAction::UseDefault => {
                    return Ok(FieldAnswer::Value(field.default_json.clone()));
                }
                FillAction::Skip => return Ok(FieldAnswer::Value(None)),
                FillAction::Cancelled => return Ok(FieldAnswer::Cancelled),
            }
        }
        let options = match field.kind {
            FieldKind::Boolean => labels(&["True", "False"]),
            FieldKind::SingleSelect => field
                .choices
                .iter()
                .enumerate()
                .map(|(index, choice)| choice_label(index, &choice.title))
                .collect::<Outcome<_>>()?,
            FieldKind::String | FieldKind::Number | FieldKind::Integer | FieldKind::MultiSelect => {
                Vec::new()
            }
        };
        let Some(answer) = self.ask_one(question, options.clone()).await? else {
            return Ok(FieldAnswer::Cancelled);
        };
        let value = match field.kind {
            FieldKind::String | FieldKind::MultiSelect => json_string(&answer),
            FieldKind::Number | FieldKind::Integer => number_answer(&answer)?,
            FieldKind::Boolean => match answer.as_str() {
                "True" => "true".to_owned(),
                "False" => "false".to_owned(),
                _ => return Err(InteractionError::InvalidAnswer),
            },
            FieldKind::SingleSelect => options
                .iter()
                .zip(&field.choices)
                .find(|(option, _)| option.label == answer)
                .map(|(_, choice)| json_string(&choice.value))
                .ok_or(InteractionError::InvalidAnswer)?,
        };
        validate_field_json(field, &value).map_err(|error| match error {
            ElicitationError::InvalidResponse => InteractionError::InvalidAnswer,
            other => InteractionError::Elicitation(other),
        })?;
        Ok(FieldAnswer::Value(Some(value)))
    }

    async fn answer_compact_field(
        &self,
        server: &str,
        message: &str,
        field: &Field,
    ) -> Outcome<FieldAnswer> {
        let name = terminal_safe(field.display_name())?;
        let description = terminal_safe(field.description.as_deref().unwrap_or(""))?;
        let question = format!(
            "MCP server {server} requests {name}{}\n{description}\nReason: {message}\nChoose an option to submit.",
            necessity(field)
        );
        let mut options = Vec::new();
        let mut actions = Vec::new();
        for (index, choice) in field.choices.iter().enumerate() {
            options.push(choice_label(index, &choice.title)?);
            actions.push(CompactAction::Value(Some(json_string(&choice.value))));
        }
        if let Some(default) = &field.default_json {
            options.push(described("Use default", Some(terminal_safe(default)?)));
            actions.push(CompactAction::Value(Some(default.clone())));
        }
        if !field.required {
            options.push(described(
                "Skip",
                Some("Submit without this optional field".to_owned()),
            ));
            actions.push(CompactAction::Value(None));
        }
        options.extend(labels(&["Decline", "Cancel"]));
        actions.extend([CompactAction::Decline, CompactAction::Cancel]);
        let answer = match self
            .ask_single(question, options, QuestionSubmission::Choice)
            .await?
        {
            Single::Cancelled => return Ok(FieldAnswer::Cancelled),
            Single::Invalid => return Err(InteractionError::InvalidAnswer),
            Single::Answer(answer) => answer,
        };
        let action = chosen_option(&answer)
            .and_then(|index| actions.into_iter().nth(index))
            .ok_or(InteractionError::InvalidAnswer)?;
        let json = match action {
            CompactAction::Decline => return Ok(FieldAnswer::Declined),
            CompactAction::Cancel => return Ok(FieldAnswer::Cancelled),
            CompactAction::Value(json) => json,
        };
        if let Some(value) = &json {
            validate_field_json(field, value).map_err(|error| match error {
                ElicitationError::InvalidResponse => InteractionError::InvalidAnswer,
                other => InteractionError::Elicitation(other),
            })?;
        }
        Ok(FieldAnswer::Value(json))
    }

    async fn answer_url(&self, request: &UrlRequest) -> Outcome<Answer> {
        let server = terminal_safe(self.server_name)?;
        let host = terminal_safe_bytes(&request.host)?;
        let url = terminal_safe(&request.url)?;
        let warning = match classify_host(&request.host) {
            HostClassification::Ordinary => "",
            HostClassification::Punycode => {
                "\nWarning: This host uses Punycode and may disguise its destination."
            }
            HostClassification::NonAscii => {
                "\nWarning: This host contains non-ASCII characters and may be visually ambiguous."
            }
        };
        let question = format!(
            "MCP server {server} requests an external browser action. Target host: {host}\nComplete URL: {url}{warning}\nOpen it? oh-fx will not fetch this URL or see browser contents."
        );
        match self
            .ask_one(question, labels(&["Open URL", "Decline"]))
            .await?
            .as_deref()
        {
            None => return Ok(Answer::Cancel),
            Some("Decline") => return Ok(Answer::Decline),
            Some("Open URL") => {}
            Some(_) => return Err(InteractionError::InvalidAnswer),
        }
        if self.open(&request.url).await {
            return Ok(Answer::AcceptUrl);
        }
        let failure = format!(
            "oh-fx could not open the browser for {host}. The URL was not fetched. Continue manually, retry the browser, or cancel?"
        );
        for _ in 0..BROWSER_RETRIES {
            let options = labels(&["Continue manually", "Retry browser", "Cancel"]);
            let decision = match self
                .ask_single(failure.clone(), options, QuestionSubmission::Label)
                .await?
            {
                Single::Cancelled => return Ok(Answer::Cancel),
                Single::Invalid => continue,
                Single::Answer(decision) => decision,
            };
            match decision.as_str() {
                "Continue manually" => return Ok(Answer::AcceptUrl),
                "Cancel" => return Ok(Answer::Cancel),
                "Retry browser" if self.open(&request.url).await => return Ok(Answer::AcceptUrl),
                _ => {}
            }
        }
        Ok(Answer::Cancel)
    }

    async fn open(&self, url: &str) -> bool {
        let browser = Arc::clone(&self.responder.browser);
        let url = url.to_owned();
        tokio::task::spawn_blocking(move || browser(&url))
            .await
            .unwrap_or(false)
    }

    async fn keep_current_value(
        &self,
        server: &str,
        field: &Field,
        current: &Current,
    ) -> Outcome<Option<bool>> {
        let name = terminal_safe(field.display_name())?;
        let value = match &current.json {
            Some(json) => terminal_safe(json)?,
            None => "(skipped)".to_owned(),
        };
        let question = format!(
            "MCP server {server}: current value for {name} is {value}. Keep it or edit it?"
        );
        let options = labels(&["Keep current", "Edit value", "Cancel"]);
        match self.ask_one(question, options).await?.as_deref() {
            None | Some("Cancel") => Ok(None),
            Some("Keep current") => Ok(Some(true)),
            Some("Edit value") => Ok(Some(false)),
            Some(_) => Err(InteractionError::InvalidAnswer),
        }
    }

    async fn answer_multi_select(
        &self,
        server: &str,
        message: &str,
        field: &Field,
    ) -> Outcome<FieldAnswer> {
        let name = terminal_safe(field.display_name())?;
        if field.default_json.is_some() || !field.required {
            match self
                .fill_action(server, message, &name, field, FillKind::MultiSelect)
                .await?
            {
                FillAction::Enter => {}
                FillAction::UseDefault => {
                    return Ok(FieldAnswer::Value(field.default_json.clone()));
                }
                FillAction::Skip => return Ok(FieldAnswer::Value(None)),
                FillAction::Cancelled => return Ok(FieldAnswer::Cancelled),
            }
        }
        let mut selected = Vec::new();
        for choice in &field.choices {
            let title = terminal_safe(&choice.title)?;
            let question =
                format!("MCP server {server}: include {title} in {name}?\nReason: {message}");
            match self
                .ask_one(question, labels(&["Include", "Exclude"]))
                .await?
                .as_deref()
            {
                None => return Ok(FieldAnswer::Cancelled),
                Some("Include") => selected.push(json_string(&choice.value)),
                Some("Exclude") => {}
                Some(_) => return Err(InteractionError::InvalidAnswer),
            }
        }
        if field
            .min_items
            .is_some_and(|minimum| selected.len() < minimum)
            || field
                .max_items
                .is_some_and(|maximum| selected.len() > maximum)
        {
            return Err(InteractionError::InvalidAnswer);
        }
        Ok(FieldAnswer::Value(Some(format!(
            "[{}]",
            selected.join(",")
        ))))
    }

    async fn fill_action(
        &self,
        server: &str,
        message: &str,
        name: &str,
        field: &Field,
        kind: FillKind,
    ) -> Outcome<FillAction> {
        let question = format!(
            "MCP server {server}: choose how to fill {name}{}.\nReason: {message}",
            necessity(field)
        );
        let has_default = field.default_json.is_some();
        let mut options = Vec::new();
        if kind == FillKind::MultiSelect && has_default {
            options.push("Use default");
        }
        options.push(kind.action_label());
        if kind != FillKind::MultiSelect && has_default {
            options.push("Use default");
        }
        if !field.required {
            options.push("Skip");
        }
        let Some(answer) = self.ask_one(question, labels(&options)).await? else {
            return Ok(FillAction::Cancelled);
        };
        if answer == kind.action_label() {
            Ok(FillAction::Enter)
        } else if answer == "Use default" && has_default {
            Ok(FillAction::UseDefault)
        } else if answer == "Skip" && !field.required {
            Ok(FillAction::Skip)
        } else {
            Err(InteractionError::InvalidAnswer)
        }
    }

    async fn retry_invalid_form(&self, server: &str) -> Outcome<bool> {
        let question = format!(
            "A response for MCP server {server} did not satisfy the requested form constraints. Edit the form or cancel?"
        );
        Ok(matches!(
            self.ask_single(question, labels(&["Edit", "Cancel"]), QuestionSubmission::Label)
                .await?,
            Single::Answer(answer) if answer == "Edit"
        ))
    }

    async fn ask_one(
        &self,
        question: String,
        options: Vec<QuestionOption>,
    ) -> Outcome<Option<String>> {
        match self
            .ask_single(question, options, QuestionSubmission::Label)
            .await?
        {
            Single::Cancelled => Ok(None),
            Single::Invalid => Err(InteractionError::InvalidAnswer),
            Single::Answer(answer) => Ok(Some(answer)),
        }
    }

    async fn ask_single(
        &self,
        question: String,
        options: Vec<QuestionOption>,
        submission: QuestionSubmission,
    ) -> Outcome<Single> {
        let asked = self.responder.questioner.ask(vec![QuestionBatchEntry {
            question,
            options,
            submission,
        }]);
        let answers = tokio::select! {
            answers = timeout_at(self.deadline, asked) => {
                answers.map_err(|_| InteractionError::McpInputTimedOut)?
            }
            () = self.finished.cancelled() => None,
        };
        Ok(match answers {
            None => Single::Cancelled,
            Some(mut answers) if answers.len() == 1 => Single::Answer(answers.remove(0)),
            Some(_) => Single::Invalid,
        })
    }
}

fn necessity(field: &Field) -> &'static str {
    if field.required {
        " (required)"
    } else {
        " (optional)"
    }
}

fn chosen_option(answer: &str) -> Option<usize> {
    let value: Value = serde_json::from_str(answer).ok()?;
    let object = value.as_object().filter(|object| object.len() == 1)?;
    usize::try_from(object.get("option")?.as_u64()?).ok()
}

fn described(label: &str, description: Option<String>) -> QuestionOption {
    QuestionOption {
        label: label.to_owned(),
        description,
    }
}

fn labels(labels: &[&str]) -> Vec<QuestionOption> {
    labels
        .iter()
        .map(|label| QuestionOption {
            label: (*label).to_owned(),
            description: None,
        })
        .collect()
}

fn choice_label(index: usize, title: &str) -> Outcome<QuestionOption> {
    Ok(QuestionOption {
        label: format!("[{}] {}", index + 1, terminal_safe(title)?),
        description: None,
    })
}

fn content(form: &FormSchema, values: &[Current]) -> Vec<(String, String)> {
    form.fields
        .iter()
        .zip(values)
        .filter_map(|(field, value)| Some((field.name.clone(), value.json.clone()?)))
        .collect()
}

fn review_question(
    server: &str,
    message: &str,
    form: &FormSchema,
    values: &[Current],
) -> Outcome<String> {
    let mut question = format!(
        "Review the completed form requested by MCP server {server}.\nReason: {message}\nCurrent values:\n"
    );
    for (field, value) in form.fields.iter().zip(values) {
        let label = terminal_safe(field.display_name())?;
        let shown = match &value.json {
            Some(json) => terminal_safe(json)?,
            None => "(skipped)".to_owned(),
        };
        let _ = writeln!(question, "- {label}: {shown}");
    }
    question.push_str("Choose Submit, Edit, Decline, or Cancel.");
    Ok(question)
}

fn json_string(value: &str) -> String {
    Value::from(value).to_string()
}

fn number_answer(answer: &str) -> Outcome<String> {
    let trimmed = answer.trim_matches([' ', '\t', '\r', '\n']);
    match Number::parse(trimmed) {
        Err(NumberError::InvalidNumber) => Err(InteractionError::InvalidAnswer),
        Ok(_) | Err(NumberError::NumberLimitExceeded) => Ok(trimmed.to_owned()),
    }
}

pub(crate) fn terminal_safe(raw: &str) -> Outcome<String> {
    terminal_safe_bytes(raw.as_bytes())
}

fn terminal_safe_bytes(raw: &[u8]) -> Outcome<String> {
    let encoded = encode_terminal_safe(raw, raw.len().saturating_mul(12));
    if encoded.truncated {
        return Err(InteractionError::PresentationLimitExceeded);
    }
    Ok(encoded.text)
}

#[cfg(test)]
mod tests;
