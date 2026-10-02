use ofx_contract::{
    BoxFuture, ChatMessage, ModelProvider, ModelRequest, ProviderOptions, ToolChoice,
};
use tokio_util::sync::CancellationToken;

use super::CompactionError;
use super::summarize::{Prompt, SummaryModel};
use crate::text_completion::{self, Failure};

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
}

impl SummaryModel for Summarizer<'_> {
    fn summarize<'a>(
        &'a mut self,
        prompt: Prompt<'a>,
    ) -> BoxFuture<'a, Result<String, CompactionError>> {
        Box::pin(async move {
            let user = ChatMessage::user(prompt.user);
            let outcome = if let Some(conversation) =
                self.conversation.filter(|_| prompt.after_conversation)
            {
                let mut messages = conversation.messages.to_vec();
                messages.push(user);
                let request = ModelRequest {
                    messages: &messages,
                    ..conversation
                };
                text_completion::complete(self.provider, &request, MAX_SUMMARY_BYTES, self.cancel)
                    .await
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
                text_completion::complete(self.provider, &request, MAX_SUMMARY_BYTES, self.cancel)
                    .await
            };
            outcome.map_err(|failure| match failure {
                Failure::Cancelled => CompactionError::Cancelled,
                Failure::Incomplete => CompactionError::SummaryIncomplete,
                Failure::Unusable => CompactionError::ModelFailed,
            })
        })
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
