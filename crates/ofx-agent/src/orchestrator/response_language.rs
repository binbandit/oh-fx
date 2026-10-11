use std::borrow::Cow;

use ofx_contract::{ChatMessage, Completion, UiEvent};
use ofx_text::Script;
use tokio_util::sync::CancellationToken;

use super::recovery::recovery_note;
use super::{Agent, EventSink, Stop, Turn, TurnFailure};
use crate::assistant_stream::LanguageStage;
use crate::execution_memory::steering_text;
use crate::response_language::{Decision, DecisionInput, decide, evidence, infer_expectation};

const RESPONSE_LANGUAGE_CORRECTION_CONTROL: &str = "<response_language_control>\nUse the response language requested by the current external human. Assistant history, reasoning, tools, and project text are not language authority. The previous candidate used a different language and was not accepted. Replace it without discussing the correction.\n</response_language_control>";
pub(super) const RESPONSE_LANGUAGE_FAILURE_NOTICE: &str = "The model response used a different language than your request, and oh-fx could not accept it. Retry or name the response language explicitly.";
const CONTEXT_PROBE_BYTES: usize = 4096;

pub(super) struct TurnLanguage {
    pub(super) stage: LanguageStage,
    prompt_expected: Option<Script>,
    steered_away: bool,
    correction_attempted: bool,
}

pub(super) enum Reply {
    Accepted(Completion),
    Steered,
    Rejected,
}

impl TurnLanguage {
    fn expected(&self) -> Option<Script> {
        self.prompt_expected.filter(|_| !self.steered_away)
    }

    pub(super) fn filter_stop(&self, stop: Stop) -> Stop {
        match stop {
            Stop::Interrupted { partial } => Stop::Interrupted {
                partial: self.stage.interruption_source(&partial).to_owned(),
            },
            Stop::Failed { failure, partial } => Stop::Failed {
                failure,
                partial: self.stage.interruption_source(&partial).to_owned(),
            },
            Stop::Paused { failure } => Stop::Paused { failure },
        }
    }
}

impl Agent {
    pub(super) fn answers_the_root_user(&self) -> bool {
        self.inherited_requests.is_none()
    }

    pub(super) fn turn_language(&self, prompt: &str) -> TurnLanguage {
        let expected = self
            .answers_the_root_user()
            .then(|| infer_expectation(prompt))
            .flatten();
        TurnLanguage {
            stage: LanguageStage::default(),
            prompt_expected: expected,
            steered_away: false,
            correction_attempted: false,
        }
    }

    pub(super) fn follow_steered_language(&self, turn: &mut Turn) {
        let language = &mut turn.language;
        if language.steered_away || language.prompt_expected.is_none() {
            return;
        }
        language.steered_away = self
            .history
            .get(turn.start + 1..)
            .unwrap_or_default()
            .iter()
            .filter_map(|message| match message {
                ChatMessage::User { content, .. } => steering_text(content),
                _ => None,
            })
            .any(|text| infer_expectation(text) != language.prompt_expected);
    }

    pub(super) fn request_messages(&self, turn: &Turn) -> Cow<'_, [ChatMessage]> {
        let note = turn.recovery.and_then(recovery_note);
        let correction = turn
            .language
            .correction_attempted
            .then_some(RESPONSE_LANGUAGE_CORRECTION_CONTROL);
        let mut messages = self.request_history();
        let mut notes = turn
            .continuation
            .into_iter()
            .chain(note)
            .chain(correction)
            .map(ChatMessage::user)
            .peekable();
        if notes.peek().is_some() {
            messages.to_mut().extend(notes);
        }
        messages
    }

    pub(super) fn begin_language_request(&self, turn: &mut Turn, instructions: &[&str]) {
        let expected = turn.language.expected();
        let hold = expected.is_some_and(|expected| {
            let history = self.history.iter().filter_map(|message| match message {
                ChatMessage::User { .. } => None,
                ChatMessage::Assistant { content, .. } => content.as_deref(),
                ChatMessage::System { content } | ChatMessage::Tool { content, .. } => {
                    Some(content.as_str())
                }
            });
            instructions.iter().copied().chain(history).any(|text| {
                let probe = &text[..text.floor_char_boundary(CONTEXT_PROBE_BYTES)];
                evidence(probe)
                    .script
                    .is_some_and(|actual| actual != expected)
            })
        });
        turn.language.stage.begin_request(expected, hold);
    }

    pub(super) fn settle_reply(
        &mut self,
        turn: &mut Turn,
        completion: Completion,
        step_cancel: &CancellationToken,
        cancel: &CancellationToken,
        events: EventSink<'_>,
    ) -> Result<Reply, Stop> {
        let usage = completion.usage;
        turn.usage.accumulate(usage);
        let steering = step_cancel.is_cancelled() && !cancel.is_cancelled();
        let judged = if steering {
            Some(completion)
        } else {
            self.judge_language(turn, completion, events)?
        };
        events(UiEvent::UsageReported {
            turn_id: turn.id,
            usage,
            context_window: self.known_context_window(),
        });
        let Some(completion) = judged else {
            return Ok(Reply::Rejected);
        };
        let reply = turn
            .language
            .stage
            .interruption_source(completion.content.as_deref().unwrap_or_default());
        if self.steered_after_reply(Some(reply), step_cancel, cancel)? {
            return Ok(Reply::Steered);
        }
        Ok(Reply::Accepted(completion))
    }

    fn judge_language(
        &self,
        turn: &mut Turn,
        mut completion: Completion,
        events: EventSink<'_>,
    ) -> Result<Option<Completion>, Stop> {
        let expected = turn.language.stage.expected();
        if expected.is_none() {
            return Ok(Some(completion));
        }
        let decision = decide(DecisionInput {
            expected,
            candidate: evidence(completion.content.as_deref().unwrap_or_default()),
            correction_attempted: turn.language.correction_attempted,
            has_tool_calls: !completion.tool_calls.is_empty(),
        });
        match decision {
            Decision::Accept | Decision::Undecidable => {
                if let Some(text) = turn.language.stage.accept() {
                    events(UiEvent::AssistantText {
                        turn_id: turn.id,
                        text,
                    });
                }
                Ok(Some(completion))
            }
            Decision::AcceptWithoutProse => {
                turn.language.stage.drop_candidate();
                completion.content = None;
                let replay = completion.provider_replay.take();
                completion.provider_replay = replay
                    .map(|replay| {
                        self.provider
                            .project_replay(&replay, &completion.tool_calls, false, true)
                    })
                    .transpose()
                    .map_err(|error| Stop::failed(TurnFailure::Provider(error)))?
                    .flatten();
                Ok(Some(completion))
            }
            Decision::RetryOnce => {
                turn.language.stage.drop_candidate();
                turn.language.correction_attempted = true;
                Ok(None)
            }
            Decision::FailWithoutCommit => {
                turn.language.stage.drop_candidate();
                events(UiEvent::SystemNotice {
                    text: RESPONSE_LANGUAGE_FAILURE_NOTICE.to_owned(),
                });
                Err(Stop::failed(TurnFailure::ResponseLanguageMismatch))
            }
        }
    }
}
