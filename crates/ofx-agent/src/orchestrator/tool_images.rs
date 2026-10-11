use std::borrow::Cow;

use ofx_contract::{ChatMessage, DEFAULT_MAX_TOOL_RESULT_BYTES, ImageInputSupport, UiEvent};
use tokio_util::sync::CancellationToken;

use super::{Agent, EventSink, Stop, Turn};

const NON_NATIVE_NOTICE: &str = "[Tool images were retained but not sent: this model does not accept inline images and no vision fallback is available. Ask the user to attach the image directly or switch to a vision-capable model.]\n";
const UNKNOWN_NOTICE: &str = "[Tool images were retained but not sent: oh-fx could not confirm image input support for this model (the model is not listed in the model catalog, or the catalog is unavailable). This can recover later in the session, so a retry may succeed; otherwise ask the user to attach the image directly.]\n";
const CATALOG_UNAVAILABLE_NOTICE: &str = "Images from tools aren't reaching the model right now because image support couldn't be confirmed (model catalog unavailable). oh-fx will retry automatically as the catalog recovers.";

pub(super) fn carries_tool_images(messages: &[ChatMessage]) -> bool {
    messages
        .iter()
        .any(|message| matches!(message, ChatMessage::Tool { images, .. } if !images.is_empty()))
}

impl Agent {
    pub(super) async fn resolve_step_capabilities(
        &mut self,
        turn: &mut Turn,
        events: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), Stop> {
        if !self.has_compactable_context(turn) && !carries_tool_images(&self.history) {
            return Ok(());
        }
        self.resolve_capabilities(cancel).await?;
        if carries_tool_images(&self.history)
            && self.image_input_support() == ImageInputSupport::Unknown
            && self
                .capabilities
                .as_ref()
                .is_some_and(|known| known.catalog_unavailable)
            && !turn.notices.tool_images
        {
            turn.notices.tool_images = true;
            events(UiEvent::Operational {
                turn_id: turn.id,
                text: format!("{CATALOG_UNAVAILABLE_NOTICE}\n"),
            });
        }
        Ok(())
    }

    pub(super) fn step_messages(&self, turn: &Turn) -> Cow<'_, [ChatMessage]> {
        let messages = self.request_messages(turn);
        let support = self.image_input_support();
        if support == ImageInputSupport::Native || !carries_tool_images(&messages) {
            return messages;
        }
        let notice = if support == ImageInputSupport::NonNative {
            NON_NATIVE_NOTICE
        } else {
            UNKNOWN_NOTICE
        };
        let mut projected = messages.into_owned();
        for message in &mut projected {
            if let ChatMessage::Tool {
                content, images, ..
            } = message
                && !images.is_empty()
            {
                images.clear();
                strip_with_notice(content, notice, DEFAULT_MAX_TOOL_RESULT_BYTES);
            }
        }
        Cow::Owned(projected)
    }

    fn image_input_support(&self) -> ImageInputSupport {
        self.capabilities
            .as_ref()
            .map_or(ImageInputSupport::Unknown, |known| {
                known.model.image_input_support
            })
    }
}

fn strip_with_notice(content: &mut String, notice: &str, limit: usize) {
    let kept = content.floor_char_boundary(limit.saturating_sub(notice.len()));
    content.truncate(kept);
    content.insert_str(0, &notice[..notice.floor_char_boundary(limit)]);
}
