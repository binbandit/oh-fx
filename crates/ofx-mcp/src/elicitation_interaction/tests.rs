use std::sync::Mutex;
use std::time::Duration;

use ofx_contract::BoxFuture;
use serde_json::Value;

use super::*;
use crate::elicitation::parse_request;
use crate::legacy_url_completion::UrlCompletions;
use crate::protocol_negotiation::ElicitationWire;

#[derive(Default)]
struct Script {
    answers: Mutex<Vec<String>>,
    asked: Mutex<Vec<QuestionBatchEntry>>,
    opened: Mutex<Vec<String>>,
    open_results: Mutex<Vec<bool>>,
    hang: bool,
}

impl Script {
    fn new(answers: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(
                answers
                    .iter()
                    .rev()
                    .map(|answer| (*answer).to_owned())
                    .collect(),
            ),
            ..Self::default()
        })
    }

    fn asked(&self) -> Vec<QuestionBatchEntry> {
        self.asked.lock().unwrap().clone()
    }

    fn remaining(&self) -> usize {
        self.answers.lock().unwrap().len()
    }

    fn opening(self: Arc<Self>, results: &[bool]) -> Arc<Self> {
        *self.open_results.lock().unwrap() = results.iter().rev().copied().collect();
        self
    }

    fn opened(&self) -> Vec<String> {
        self.opened.lock().unwrap().clone()
    }

    fn responder(self: &Arc<Self>) -> InputResponder {
        let browser = Arc::clone(self);
        InputResponder::new(
            Arc::clone(self) as Arc<dyn QuestionAsker>,
            move |url: &str| {
                browser.opened.lock().unwrap().push(url.to_owned());
                browser.open_results.lock().unwrap().pop().unwrap_or(true)
            },
        )
    }
}

impl QuestionAsker for Script {
    fn ask(&self, entries: Vec<QuestionBatchEntry>) -> BoxFuture<'static, Option<Vec<String>>> {
        self.asked.lock().unwrap().extend(entries);
        if self.hang {
            return Box::pin(std::future::pending());
        }
        let answer = self.answers.lock().unwrap().pop();
        Box::pin(async move { answer.map(|answer| vec![answer]) })
    }
}

fn request(params: &str) -> Request {
    let params: Value = serde_json::from_str(params).unwrap();
    parse_request(ElicitationWire::LegacyMcp2025_11, &params).unwrap()
}

const DECLINE: &str = r#"{"action":"decline"}"#;
const CANCEL: &str = r#"{"action":"cancel"}"#;
const ACCEPT: &str = r#"{"action":"accept"}"#;

async fn respond_with(
    responder: &InputResponder,
    server_name: &str,
    params: &str,
) -> Outcome<String> {
    let finished = CancellationToken::new();
    Interaction {
        responder,
        server_name,
        deadline: Instant::now() + Duration::from_mins(1),
        finished: &finished,
    }
    .respond(&request(params))
    .await
    .map(|answer| answer.to_json())
}

async fn respond_as(script: &Arc<Script>, server_name: &str, params: &str) -> Outcome<String> {
    respond_with(&script.responder(), server_name, params).await
}

async fn respond(script: &Arc<Script>, params: &str) -> Outcome<String> {
    respond_as(script, "fixture", params).await
}

async fn respond_compactly(script: &Arc<Script>, params: &str) -> Outcome<String> {
    respond_with(&script.responder().with_compact_forms(), "fixture", params).await
}

const PROFILE: &str = r#"{"message":"Profile","requestedSchema":{"type":"object","properties":{"name":{"type":"string","minLength":3},"ratio":{"type":"number","minimum":0,"maximum":1},"age":{"type":"integer","minimum":18},"active":{"type":"boolean"},"color":{"type":"string","oneOf":[{"const":"red","title":"Red"},{"const":"blue","title":"Blue"}]},"tags":{"type":"array","items":{"type":"string","anyOf":[{"const":"a","title":"A"},{"const":"b","title":"B"}]},"minItems":1}},"required":["name","ratio","age","active","color","tags"]}}"#;

#[tokio::test]
async fn interaction_review_shows_every_validated_current_value_before_submission() {
    let script = Script::new(&[
        "A1",
        "Edit",
        "Ada",
        "0.5",
        "42",
        "True",
        "[1] Red",
        "Include",
        "Exclude",
        "Edit",
        "Edit value",
        "Bea",
        "Keep current",
        "Keep current",
        "Keep current",
        "Keep current",
        "Edit value",
        "Exclude",
        "Include",
        "Submit",
    ]);
    assert_eq!(
        respond(&script, PROFILE).await.unwrap(),
        r#"{"action":"accept","content":{"name":"Bea","ratio":0.5,"age":42,"active":true,"color":"red","tags":["b"]}}"#
    );
    let asked = script.asked();
    let review = &asked.last().unwrap().question;
    for expected in [
        "- name: \"Bea\"",
        "- ratio: 0.5",
        "- age: 42",
        "- active: true",
        "- color: \"red\"",
        "- tags: [\"b\"]",
    ] {
        assert!(review.contains(expected), "{review}");
    }
    assert_eq!(
        asked[0].question,
        "MCP server fixture requests name (required)\nReason: Profile"
    );
    assert!(asked[0].options.is_empty());
    assert_eq!(
        asked[1].question,
        "A response for MCP server fixture did not satisfy the requested form constraints. Edit the form or cancel?"
    );
    let colors: Vec<_> = asked[6]
        .options
        .iter()
        .map(|option| option.label.as_str())
        .collect();
    assert_eq!(colors, ["[1] Red", "[2] Blue"]);
    assert_eq!(
        asked[7].question,
        "MCP server fixture: include A in tags?\nReason: Profile"
    );
    assert_eq!(
        asked[10].question,
        "MCP server fixture: current value for name is \"Ada\". Keep it or edit it?"
    );
    assert_eq!(
        review,
        "Review the completed form requested by MCP server fixture.\nReason: Profile\nCurrent values:\n- name: \"Bea\"\n- ratio: 0.5\n- age: 42\n- active: true\n- color: \"red\"\n- tags: [\"b\"]\nChoose Submit, Edit, Decline, or Cancel."
    );
}

#[tokio::test]
async fn form_interaction_supports_primitive_defaults_decline_and_cancellation() {
    let defaults = r#"{"message":"Defaults","requestedSchema":{"type":"object","properties":{"note":{"type":"string","default":"default-note"},"enabled":{"type":"boolean","default":true},"tags":{"type":"array","items":{"type":"string","enum":["a","b"]},"default":["a"]}},"required":["note","enabled","tags"]}}"#;
    let accepted = Script::new(&["Use default", "Use default", "Use default", "Submit"]);
    assert_eq!(
        respond(&accepted, defaults).await.unwrap(),
        r#"{"action":"accept","content":{"note":"default-note","enabled":true,"tags":["a"]}}"#
    );
    let asked = accepted.asked();
    assert_eq!(
        asked[0].question,
        "MCP server fixture: choose how to fill note (required).\nReason: Defaults"
    );
    let fill: Vec<_> = asked[0]
        .options
        .iter()
        .map(|option| option.label.as_str())
        .collect();
    assert_eq!(fill, ["Enter value", "Use default"]);
    let multi: Vec<_> = asked[2]
        .options
        .iter()
        .map(|option| option.label.as_str())
        .collect();
    assert_eq!(multi, ["Use default", "Choose values"]);
    let declined = Script::new(&["Use default", "Use default", "Use default", "Decline"]);
    assert_eq!(respond(&declined, defaults).await.unwrap(), DECLINE);
    let cancelled = Script::new(&[]);
    assert_eq!(respond(&cancelled, defaults).await.unwrap(), CANCEL);
}

#[tokio::test]
async fn form_controls_are_separate_from_literal_skip_and_use_default_values() {
    let script = Script::new(&[
        "Enter value",
        "Skip",
        "Enter value",
        "Use default",
        "Submit",
    ]);
    assert_eq!(
        respond(
            &script,
            r#"{"message":"Literal controls","requestedSchema":{"type":"object","properties":{"literalSkip":{"type":"string"},"literalDefault":{"type":"string","default":"fallback"}},"required":["literalDefault"]}}"#
        )
        .await
        .unwrap(),
        r#"{"action":"accept","content":{"literalSkip":"Skip","literalDefault":"Use default"}}"#
    );
    let skip: Vec<_> = script.asked()[0]
        .options
        .iter()
        .map(|option| option.label.clone())
        .collect();
    assert_eq!(skip, ["Enter value", "Skip"]);
}

#[tokio::test]
async fn single_select_local_identities_preserve_every_colliding_wire_value() {
    let params = r#"{"message":"Pick","requestedSchema":{"type":"object","properties":{"choice":{"type":"string","oneOf":[{"const":"Skip","title":"Skip"},{"const":"Use default","title":"Use default"},{"const":"value-3","title":"Duplicate"},{"const":"Duplicate","title":"Duplicate"},{"const":"escape-raw","title":"Collision\u001b[2J"},{"const":"escape-literal","title":"Collision\\x1b[2J"}]}},"required":["choice"]}}"#;
    let wire_values = [
        "Skip",
        "Use default",
        "value-3",
        "Duplicate",
        "escape-raw",
        "escape-literal",
    ];
    let probe = Script::new(&[]);
    assert_eq!(respond(&probe, params).await.unwrap(), CANCEL);
    let labels: Vec<_> = probe.asked()[0]
        .options
        .iter()
        .map(|option| option.label.clone())
        .collect();
    assert!(labels[4].ends_with("Collision\\x1b[2J"));
    assert!(labels[5].ends_with("Collision\\x1b[2J"));
    for (index, label) in labels.iter().enumerate() {
        assert!(!labels[index + 1..].contains(label));
        let script = Script::new(&[label, "Submit"]);
        assert_eq!(
            respond(&script, params).await.unwrap(),
            format!(
                r#"{{"action":"accept","content":{{"choice":{}}}}}"#,
                json_string(wire_values[index])
            )
        );
    }
    for title in ["Skip", "Use default", "Duplicate", "escape-raw"] {
        let script = Script::new(&[title, "Cancel"]);
        assert_eq!(respond(&script, params).await.unwrap(), CANCEL);
    }
}

#[tokio::test]
async fn elicitation_presentation_terminal_safes_every_untrusted_display_value() {
    let script = Script::new(&[
        "value\u{1b}]52;c;answer\u{7}\nnext",
        "[1] Choice\\x1b]52;c;option\\x07",
        "Submit",
    ]);
    let params = r#"{"message":"Line\nInjected\u001b]52;c;message\u0007\u0085","requestedSchema":{"type":"object","properties":{"value":{"type":"string","title":"Field\u001b]52;c;title\u0007","description":"Description\u001b[2J"},"choice":{"type":"string","oneOf":[{"const":"exact","title":"Choice\u001b]52;c;option\u0007"}]}} ,"required":["value","choice"]}}"#;
    assert_eq!(
        respond_as(&script, "server\u{1b}]52;c;name\u{7}", params)
            .await
            .unwrap(),
        r#"{"action":"accept","content":{"value":"value\u001b]52;c;answer\u0007\nnext","choice":"exact"}}"#
    );
    let asked = script.asked();
    for entry in &asked {
        assert!(!entry.question.contains('\u{1b}') && !entry.question.contains('\u{85}'));
        assert!(!entry.question.contains("Line\nInjected"));
        assert!(
            entry
                .options
                .iter()
                .all(|option| !option.label.contains('\u{1b}'))
        );
    }
    let shown = asked
        .iter()
        .flat_map(|entry| {
            std::iter::once(entry.question.clone())
                .chain(entry.options.iter().map(|option| option.label.clone()))
        })
        .collect::<Vec<_>>()
        .join("\n");
    for fragment in [
        "server\\x1b]52;c;name\\x07",
        "Line\\x0aInjected",
        "Field\\x1b]52;c;title\\x07",
        "Choice\\x1b]52;c;option\\x07",
        "\\u{0085}",
        "Description\\x1b[2J",
    ] {
        assert!(shown.contains(fragment), "{fragment}");
    }
    assert_eq!(
        terminal_safe("bad\u{1b}]52;c;x\u{7}\u{85}\nline").unwrap(),
        "bad\\x1b]52;c;x\\x07\\u{0085}\\x0aline"
    );
}

#[tokio::test]
async fn unrecognized_reviews_start_a_new_attempt_until_the_limit() {
    let params = r#"{"message":"Loop","requestedSchema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}}"#;
    let mut answers = vec!["x", "Other"];
    for _ in 1..MAX_FORM_ATTEMPTS {
        answers.extend(["Keep current", "Other"]);
    }
    let script = Script::new(&answers);
    assert_eq!(
        respond(&script, params).await,
        Err(InteractionError::RetryLimitExceeded)
    );
    assert_eq!(script.remaining(), 0);
    let edited = Script::new(&["x", "Edit", "Edit value", "y", "Submit"]);
    assert_eq!(
        respond(&edited, params).await.unwrap(),
        r#"{"action":"accept","content":{"value":"y"}}"#
    );
}

#[tokio::test]
async fn invalid_answers_ask_to_edit_or_cancel_and_numbers_keep_their_lexeme() {
    let params = r#"{"message":"Count","requestedSchema":{"type":"object","properties":{"count":{"type":"integer","maximum":1e3}},"required":["count"]}}"#;
    let script = Script::new(&["1e4", "Edit", " 1E2 ", "Submit"]);
    assert_eq!(
        respond(&script, params).await.unwrap(),
        r#"{"action":"accept","content":{"count":1E2}}"#
    );
    let refused = Script::new(&["many", "Cancel"]);
    assert_eq!(respond(&refused, params).await.unwrap(), CANCEL);
    assert_eq!(refused.remaining(), 0);
}

#[tokio::test]
async fn an_unanswered_question_times_out_or_ends_with_its_operation() {
    let hanging = Arc::new(Script {
        hang: true,
        ..Script::default()
    });
    let responder = hanging.responder();
    let params = request(
        r#"{"message":"Wait","requestedSchema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}}"#,
    );
    let finished = CancellationToken::new();
    let late = Interaction {
        responder: &responder,
        server_name: "fixture",
        deadline: Instant::now(),
        finished: &finished,
    };
    assert_eq!(
        late.respond(&params).await,
        Err(InteractionError::McpInputTimedOut)
    );
    finished.cancel();
    let ended = Interaction {
        deadline: Instant::now() + Duration::from_mins(1),
        ..late
    };
    assert_eq!(ended.respond(&params).await, Ok(Answer::Cancel));
    assert_eq!(
        InteractionError::McpInputTimedOut.to_string(),
        "McpInputTimedOut"
    );
    assert_eq!(
        InteractionError::Elicitation(ElicitationError::LimitExceeded).to_string(),
        "LimitExceeded"
    );
}

fn reviewed(script: &Script) -> bool {
    script
        .asked()
        .iter()
        .any(|entry| entry.question.contains("Current values:"))
}

#[tokio::test]
async fn compact_form_submits_a_choice_or_declines_without_a_review() {
    let params = r#"{"message":"Name","requestedSchema":{"type":"object","properties":{"name":{"type":"string","enum":["Decline"]}},"required":["name"]}}"#;
    for (answer, expected) in [
        (
            r#"{"option":0}"#,
            r#"{"action":"accept","content":{"name":"Decline"}}"#,
        ),
        (r#"{"option":1}"#, DECLINE),
        (r#"{"option":2}"#, CANCEL),
    ] {
        let script = Script::new(&[answer]);
        assert_eq!(respond_compactly(&script, params).await.unwrap(), expected);
        assert_eq!(script.remaining(), 0);
        assert!(!reviewed(&script));
        let asked = script.asked();
        assert_eq!(asked[0].submission, QuestionSubmission::Choice);
        assert_eq!(
            asked[0].question,
            "MCP server fixture requests name (required)\n\nReason: Name\nChoose an option to submit."
        );
        let options: Vec<_> = asked[0]
            .options
            .iter()
            .map(|option| option.label.as_str())
            .collect();
        assert_eq!(options, ["[1] Decline", "Decline", "Cancel"]);
    }
    for answer in [r#"{"option":3}"#, "Decline", r#"{"option":0,"extra":1}"#] {
        let script = Script::new(&[answer, "Cancel"]);
        assert_eq!(respond_compactly(&script, params).await.unwrap(), CANCEL);
    }
}

#[tokio::test]
async fn compact_form_defaults_skip_and_choice_labels_preserve_exact_values() {
    let params = r#"{"message":"Choose","requestedSchema":{"type":"object","properties":{"name":{"type":"string","description":"Pick one","default":"b","oneOf":[{"const":"a","title":"Decline"},{"const":"b","title":"Cancel"},{"const":"c","title":"Decline"}]}}}}"#;
    for (answer, content) in [
        (r#"{"option":0}"#, r#"{"name":"a"}"#),
        (r#"{"option":1}"#, r#"{"name":"b"}"#),
        (r#"{"option":2}"#, r#"{"name":"c"}"#),
        (r#"{"option":3}"#, r#"{"name":"b"}"#),
        (r#"{"option":4}"#, "{}"),
    ] {
        let script = Script::new(&[answer]);
        assert_eq!(
            respond_compactly(&script, params).await.unwrap(),
            format!(r#"{{"action":"accept","content":{content}}}"#)
        );
        assert_eq!(script.remaining(), 0);
        let asked = script.asked();
        assert_eq!(
            asked[0].question,
            "MCP server fixture requests name (optional)\nPick one\nReason: Choose\nChoose an option to submit."
        );
        let options: Vec<_> = asked[0]
            .options
            .iter()
            .map(|option| (option.label.as_str(), option.description.as_deref()))
            .collect();
        assert_eq!(
            options,
            [
                ("[1] Decline", None),
                ("[2] Cancel", None),
                ("[3] Decline", None),
                ("Use default", Some("\"b\"")),
                ("Skip", Some("Submit without this optional field")),
                ("Decline", None),
                ("Cancel", None),
            ]
        );
    }
}

#[tokio::test]
async fn compact_form_keeps_review_for_short_text_numbers_and_booleans() {
    for (kind, answer, value) in [
        ("string", "x", "\"x\""),
        ("number", "0.5", "0.5"),
        ("integer", "1", "1"),
        ("boolean", "True", "true"),
    ] {
        let params = format!(
            r#"{{"message":"Value","requestedSchema":{{"type":"object","properties":{{"value":{{"type":"{kind}"}}}},"required":["value"]}}}}"#
        );
        let script = Script::new(&[answer, "Submit"]);
        assert_eq!(
            respond_compactly(&script, &params).await.unwrap(),
            format!(r#"{{"action":"accept","content":{{"value":{value}}}}}"#)
        );
        assert!(reviewed(&script));
        assert_eq!(script.remaining(), 0);
    }
}

#[tokio::test]
async fn compact_form_keeps_review_for_four_choices() {
    let script = Script::new(&["[1] a", "Submit"]);
    assert_eq!(
        respond_compactly(
            &script,
            r#"{"message":"Choose","requestedSchema":{"type":"object","properties":{"name":{"type":"string","enum":["a","b","c","d"]}},"required":["name"]}}"#
        )
        .await
        .unwrap(),
        r#"{"action":"accept","content":{"name":"a"}}"#
    );
    assert!(reviewed(&script));
    assert_eq!(script.asked()[0].submission, QuestionSubmission::Label);
}

const AUTHORIZE: &str = r#"{"mode":"url","message":"Authorize","url":"https://example.test/full?state=opaque","elicitationId":"url-1"}"#;

#[tokio::test]
async fn url_interaction_requires_consent_and_does_not_open_on_decline() {
    let script = Script::new(&["Decline"]);
    assert_eq!(
        respond(
            &script,
            r#"{"mode":"url","message":"Authorize","url":"https://xn--pple-43d.test/full?state=opaque","elicitationId":"url-1"}"#
        )
        .await
        .unwrap(),
        DECLINE
    );
    assert!(script.opened().is_empty());
    let asked = script.asked();
    assert_eq!(
        asked[0].question,
        "MCP server fixture requests an external browser action. Target host: xn--pple-43d.test\nComplete URL: https://xn--pple-43d.test/full?state=opaque\nWarning: This host uses Punycode and may disguise its destination.\nOpen it? oh-fx will not fetch this URL or see browser contents."
    );
    let options: Vec<_> = asked[0]
        .options
        .iter()
        .map(|option| option.label.as_str())
        .collect();
    assert_eq!(options, ["Open URL", "Decline"]);
    let cancelled = Script::new(&[]);
    assert_eq!(respond(&cancelled, AUTHORIZE).await.unwrap(), CANCEL);
    let opened = Script::new(&["Open URL"]);
    assert_eq!(respond(&opened, AUTHORIZE).await.unwrap(), ACCEPT);
    assert_eq!(opened.opened(), ["https://example.test/full?state=opaque"]);
    let unknown = Script::new(&["maybe"]);
    assert_eq!(
        respond(&unknown, AUTHORIZE).await,
        Err(InteractionError::InvalidAnswer)
    );
}

#[tokio::test]
async fn url_browser_failure_supports_manual_continuation_and_bounded_retry() {
    let manual = Script::new(&["Open URL", "Continue manually"]).opening(&[false]);
    assert_eq!(respond(&manual, AUTHORIZE).await.unwrap(), ACCEPT);
    assert_eq!(manual.opened().len(), 1);
    assert_eq!(
        manual.asked()[1].question,
        "oh-fx could not open the browser for example.test. The URL was not fetched. Continue manually, retry the browser, or cancel?"
    );
    let retry = Script::new(&["Open URL", "Retry browser"]).opening(&[false, true]);
    assert_eq!(respond(&retry, AUTHORIZE).await.unwrap(), ACCEPT);
    assert_eq!(retry.opened().len(), 2);
    let exhausted = Script::new(&["Open URL", "Retry browser", "Retry browser", "Other"])
        .opening(&[false, false, false]);
    assert_eq!(respond(&exhausted, AUTHORIZE).await.unwrap(), CANCEL);
    assert_eq!(exhausted.opened().len(), 3);
    let cancelled = Script::new(&["Open URL", "Cancel"]).opening(&[false]);
    assert_eq!(respond(&cancelled, AUTHORIZE).await.unwrap(), CANCEL);
    let unknown = Script::new(&["Open URL", "maybe", "Continue manually"]).opening(&[false]);
    assert_eq!(respond(&unknown, AUTHORIZE).await.unwrap(), ACCEPT);
    assert_eq!(unknown.asked().len(), 3);
    assert_eq!(unknown.opened().len(), 1);
}

#[tokio::test]
async fn url_hosts_are_shown_terminal_safe_with_their_warning() {
    let script = Script::new(&["Decline"]);
    respond(
        &script,
        r#"{"mode":"url","message":"Authorize","url":"https://ex%C3%A4mple.test/\u001b[2J","elicitationId":"url-1"}"#,
    )
    .await
    .unwrap();
    let question = &script.asked()[0].question;
    assert!(
        question.contains("Target host: ex\u{e4}mple.test\nComplete URL: https://ex%C3%A4mple.test/\\x1b[2J\nWarning: This host contains non-ASCII characters and may be visually ambiguous."),
        "{question}"
    );
    assert_eq!(classify_host(b"example.test"), HostClassification::Ordinary);
    assert_eq!(
        classify_host(b"XN--e1awd7f.test"),
        HostClassification::Punycode
    );
    assert_eq!(
        classify_host(b"login.xn--pple-43d.test"),
        HostClassification::Punycode
    );
    assert_eq!(
        classify_host("ex\u{e4}mple.test".as_bytes()),
        HostClassification::NonAscii
    );
}

fn url_required(ids: &[&str]) -> Vec<(String, UrlRequest)> {
    ids.iter()
        .map(|id| {
            let RequestKind::Url(url) = request(&format!(
                r#"{{"mode":"url","message":"Connect {id}","url":"https://{id}.test/","elicitationId":"{id}"}}"#
            ))
            .kind
            else {
                panic!("a URL request");
            };
            ((*id).to_owned(), url)
        })
        .collect()
}

fn interaction<'a>(
    responder: &'a InputResponder,
    finished: &'a CancellationToken,
) -> Interaction<'a> {
    Interaction {
        responder,
        server_name: "fixture",
        deadline: Instant::now() + Duration::from_mins(1),
        finished,
    }
}

#[tokio::test]
async fn url_consent_covers_every_request_and_a_cancel_ends_it() {
    let finished = CancellationToken::new();
    let accepted = Script::new(&["Open URL", "Open URL"]);
    let responder = accepted.responder();
    assert_eq!(
        interaction(&responder, &finished)
            .consent_to_urls(&url_required(&["one", "two"]))
            .await,
        Ok(UrlConsent::Accepted)
    );
    assert_eq!(
        accepted.opened(),
        ["https://one.test/", "https://two.test/"]
    );
    let declined = Script::new(&["Decline", "Open URL"]);
    let responder = declined.responder();
    assert_eq!(
        interaction(&responder, &finished)
            .consent_to_urls(&url_required(&["one", "two"]))
            .await,
        Ok(UrlConsent::Declined)
    );
    assert_eq!(declined.asked().len(), 2);
    let cancelled = Script::new(&[]);
    let responder = cancelled.responder();
    assert_eq!(
        interaction(&responder, &finished)
            .consent_to_urls(&url_required(&["one", "two"]))
            .await,
        Ok(UrlConsent::Cancelled)
    );
    assert_eq!(cancelled.asked().len(), 1);
}

#[tokio::test]
async fn legacy_url_completion_is_separate_from_consent_and_supports_manual_retry() {
    let finished = CancellationToken::new();
    let completions = UrlCompletions::default();
    let ids = ["first".to_owned(), "second".to_owned()];
    let manual = Script::new(&["I completed it / Retry"]);
    let responder = manual.responder();
    let waiter = completions.wait_for(&ids).unwrap();
    assert_eq!(
        interaction(&responder, &finished)
            .await_completion(&waiter)
            .await,
        Ok(true)
    );
    let asked = manual.asked();
    assert_eq!(
        asked[0].question,
        "Complete the browser flow requested by MCP server fixture. oh-fx will continue automatically if the server confirms every URL request. Otherwise choose I completed it / Retry, or Cancel."
    );
    assert!(manual.opened().is_empty());
    let cancelled = Script::new(&["Cancel"]);
    let responder = cancelled.responder();
    assert_eq!(
        interaction(&responder, &finished)
            .await_completion(&waiter)
            .await,
        Ok(false)
    );
    let unknown = Script::new(&["maybe"]);
    let responder = unknown.responder();
    assert_eq!(
        interaction(&responder, &finished)
            .await_completion(&waiter)
            .await,
        Err(InteractionError::InvalidAnswer)
    );
    let waiting = Arc::new(Script {
        hang: true,
        ..Script::default()
    });
    let responder = waiting.responder();
    let asking = interaction(&responder, &finished);
    let awaited = asking.await_completion(&waiter);
    tokio::pin!(awaited);
    tokio::select! {
        biased;
        _ = &mut awaited => panic!("nothing completed yet"),
        () = std::future::ready(()) => {}
    }
    completions.record("first");
    completions.record("second");
    assert_eq!(awaited.await, Ok(true));
}

#[tokio::test]
async fn a_closed_connection_ends_the_completion_wait_as_cancelled() {
    let finished = CancellationToken::new();
    let completions = UrlCompletions::default();
    let waiter = completions.wait_for(&["open".to_owned()]).unwrap();
    let waiting = Arc::new(Script {
        hang: true,
        ..Script::default()
    });
    let responder = waiting.responder();
    let asking = interaction(&responder, &finished);
    let awaited = asking.await_completion(&waiter);
    tokio::pin!(awaited);
    tokio::select! {
        biased;
        _ = &mut awaited => panic!("the connection is still open"),
        () = std::future::ready(()) => {}
    }
    completions.close();
    assert_eq!(awaited.await, Ok(false));
    assert_eq!(waiting.asked().len(), 1);
    let unasked = Script::new(&["I completed it / Retry"]);
    let responder = unasked.responder();
    assert_eq!(
        interaction(&responder, &finished)
            .await_completion(&waiter)
            .await,
        Ok(false)
    );
    assert!(unasked.asked().is_empty());
}
