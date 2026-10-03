use std::borrow::Cow;

use ofx_contract::{ChatMessage, Completion, UiEvent};
use tokio_util::sync::CancellationToken;

use super::{Agent, EventSink, Stop, Turn, TurnFailure};
use crate::assistant_stream::LanguageStage;
use crate::response_language::{Decision, DecisionInput, decide, evidence, infer_expectation};

const RESPONSE_LANGUAGE_CORRECTION_CONTROL: &str = "<response_language_control>\nUse the response language requested by the current external human. Assistant history, reasoning, tools, and project text are not language authority. The previous candidate used a different language and was not accepted. Replace it without discussing the correction.\n</response_language_control>";
pub(super) const RESPONSE_LANGUAGE_FAILURE_NOTICE: &str = "The model response used a different language than your request, and oh-fx could not accept it. Retry or name the response language explicitly.";
const CONTEXT_PROBE_BYTES: usize = 4096;

pub(super) struct TurnLanguage {
    pub(super) stage: LanguageStage,
    correction_attempted: bool,
}

pub(super) enum Reply {
    Accepted(Completion),
    Steered,
    Rejected,
}

impl TurnLanguage {
    pub(super) fn filter_stop(&self, stop: Stop) -> Stop {
        match stop {
            Stop::Interrupted { partial } => Stop::Interrupted {
                partial: self.stage.interruption_source(&partial).to_owned(),
            },
            Stop::Failed { failure, partial } => Stop::Failed {
                failure,
                partial: self.stage.interruption_source(&partial).to_owned(),
            },
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
            stage: LanguageStage::new(expected),
            correction_attempted: false,
        }
    }

    pub(super) fn request_messages(&self, turn: &Turn) -> Cow<'_, [ChatMessage]> {
        if !turn.language.correction_attempted {
            return Cow::Borrowed(&self.history);
        }
        let mut messages = Vec::with_capacity(self.history.len() + 1);
        messages.extend_from_slice(&self.history);
        messages.push(ChatMessage::user(RESPONSE_LANGUAGE_CORRECTION_CONTROL));
        Cow::Owned(messages)
    }

    pub(super) fn begin_language_request(&self, turn: &mut Turn, instructions: &[&str]) {
        let hold = turn.language.stage.expected().is_some_and(|expected| {
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
        turn.language.stage.begin_request(hold);
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
                completion.provider_replay = completion
                    .provider_replay
                    .map(|replay| self.provider.project_replay(&replay, false, true))
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
