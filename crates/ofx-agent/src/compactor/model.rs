use ofx_contract::{
    BoxFuture, ChatMessage, ModelProvider, ModelRequest, ProviderOptions, ToolChoice,
};
use tokio_util::sync::CancellationToken;

use super::CompactionError;
use super::summarize::{Prompt, SummaryModel};
use super::trace::{CompactionTraceKind, Optional, Tracer};
use crate::text_completion::{self, Failure, Outcome, Reason};

const MAX_SUMMARY_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct Summarizer<'a> {
    pub(crate) provider: &'a dyn ModelProvider,
    pub(crate) model: &'a str,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) options: ProviderOptions<'a>,
    pub(crate) reasoning_efforts: &'a [String],
    pub(crate) conversation: Option<ModelRequest<'a>>,
    pub(crate) session_id: Option<&'a str>,
    pub(crate) cancel: &'a CancellationToken,
    pub(crate) trace: Tracer,
}

impl SummaryModel for Summarizer<'_> {
    fn summarize<'a>(
        &'a mut self,
        prompt: Prompt<'a>,
    ) -> BoxFuture<'a, Result<String, CompactionError>> {
        Box::pin(async move {
            let user = ChatMessage::user(prompt.user);
            let (model, after_conversation, outcome) = if let Some(conversation) =
                self.conversation.filter(|_| prompt.after_conversation)
            {
                let mut messages = conversation.messages.to_vec();
                messages.push(user);
                let request = ModelRequest {
                    messages: &messages,
                    ..conversation
                };
                let outcome = text_completion::complete(
                    self.provider,
                    &request,
                    MAX_SUMMARY_BYTES,
                    self.cancel,
                )
                .await;
                (conversation.model, true, outcome)
            } else {
                let instructions = [prompt.system];
                let messages = [user];
                let request = ModelRequest {
                    model: self.model,
                    instructions: &instructions,
                    messages: &messages,
                    tools: &[],
                    tool_choice: ToolChoice::None,
                    max_output_tokens: self.max_output_tokens,
                    provider_options: ProviderOptions {
                        reasoning_effort: lowest_reasoning_effort(self.reasoning_efforts),
                        ..self.options
                    },
                    session_id: self.session_id,
                };
                let outcome = text_completion::complete(
                    self.provider,
                    &request,
                    MAX_SUMMARY_BYTES,
                    self.cancel,
                )
                .await;
                (self.model, false, outcome)
            };
            let Ok(Outcome { reply, usage }) = outcome else {
                return Err(CompactionError::Cancelled);
            };
            self.trace.log(
                false,
                format_args!(
                    "compaction model call model={model} after_conversation={after_conversation} input_tokens={} cache_read_tokens=null cache_write_tokens=null output_tokens={}",
                    Optional(usage.input_tokens),
                    Optional(usage.output_tokens),
                ),
            );
            reply.map_err(|failure| self.failed(model, &failure))
        })
    }
}

impl Summarizer<'_> {
    fn failed(&self, model: &str, failure: &Failure) -> CompactionError {
        let detail = &failure.detail;
        match failure.reason {
            Reason::Transport | Reason::Provider => self.trace.failure(
                CompactionTraceKind::SummaryTransportFailed,
                format_args!("model={model} {detail}"),
            ),
            Reason::ToolCall => self.trace.failure(
                CompactionTraceKind::SummaryToolCallRejected,
                format_args!("model={model}"),
            ),
            Reason::Incomplete => {
                self.trace.failure(
                    CompactionTraceKind::SummaryIncomplete,
                    format_args!("model={model} {detail}"),
                );
                return CompactionError::SummaryIncomplete;
            }
            Reason::Truncated => self.trace.failure(
                CompactionTraceKind::SummaryTruncated,
                format_args!("model={model} {detail}"),
            ),
        }
        CompactionError::ModelFailed
    }
}

fn lowest_reasoning_effort(efforts: &[String]) -> Option<&str> {
    efforts
        .iter()
        .find(|effort| *effort == "none")
        .or_else(|| efforts.first())
        .map(String::as_str)
}

#[cfg(test)]
mod tests;
