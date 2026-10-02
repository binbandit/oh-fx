use ofx_contract::{ChatMessage, ModelRequest, ProviderOptions};
use tokio_util::sync::CancellationToken;

use super::Agent;
use crate::compactor::{self, Compacted, CompactionError, Size, Summarizer};
use crate::execution_memory::{history_turns, retain};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compaction {
    Compacted,
    Unchanged,
}

impl Agent {
    pub async fn compact(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<Compaction, CompactionError> {
        if self.resolve_capabilities(cancel).await.is_err() {
            return Err(CompactionError::Cancelled);
        }
        let options = self
            .capabilities
            .as_ref()
            .map_or_else(ProviderOptions::default, |known| {
                known.model.provider_options(
                    self.config.reasoning_effort.as_deref(),
                    self.config.fast_mode,
                )
            });
        let compacted = self
            .compacted_history(self.compaction_size(), false, options, None, cancel)
            .await?;
        Ok(match compacted {
            Some(compacted) => {
                self.install_compaction(compacted);
                Compaction::Compacted
            }
            None => Compaction::Unchanged,
        })
    }

    fn compaction_size(&self) -> Size {
        let context_window = self
            .capabilities
            .as_ref()
            .and_then(|known| known.model.context_window);
        Size::of(
            context_window,
            self.config.max_output_tokens,
            self.config.auto_compact_percent,
        )
    }

    async fn compacted_history(
        &self,
        size: Size,
        active: bool,
        options: ProviderOptions<'_>,
        conversation: Option<ModelRequest<'_>>,
        cancel: &CancellationToken,
    ) -> Result<Option<Compacted>, CompactionError> {
        let turns = history_turns(&self.history, &self.turn_starts);
        let reasoning_efforts = self
            .capabilities
            .as_ref()
            .map_or(&[][..], |known| &known.model.reasoning_efforts);
        let mut summarizer = Summarizer {
            provider: &*self.provider,
            model: &self.config.model,
            max_output_tokens: self.config.max_output_tokens,
            options,
            reasoning_efforts,
            conversation,
            cancel,
        };
        let request = compactor::Request {
            turns: &turns,
            active,
            earlier: self.compacted.as_ref(),
            size,
            model: &self.config.model,
            sends_after_conversation: conversation.is_some(),
        };
        compactor::compact(request, &mut summarizer, cancel).await
    }

    fn install_compaction(&mut self, compacted: Compacted) {
        retain(
            &mut self.history,
            &mut self.turn_starts,
            compacted.cut,
            ChatMessage::user(compacted.text),
        );
        self.compacted = Some(compacted.payload);
    }
}
