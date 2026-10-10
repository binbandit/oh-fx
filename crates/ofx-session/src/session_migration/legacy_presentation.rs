use crate::json_fields::{Fields, Json};
use crate::session_codec::recovery_checkpoint::{durable_text, list, tag};
use crate::session_event::{CommittedFilePresentation, LifecycleId, PresentationLine};

pub(super) enum CommandReplay {
    Available { handle: String, framed_bytes: u64 },
    Unavailable,
}

pub(super) struct CancelledCommand {
    pub(super) replay: Option<CommandReplay>,
    pub(super) artifact: Option<String>,
}

impl CommandReplay {
    pub(super) fn available(replay: Option<&Self>) -> (Option<String>, Option<u64>) {
        match replay {
            Some(Self::Available {
                handle,
                framed_bytes,
            }) => (Some(handle.clone()), Some(*framed_bytes)),
            Some(Self::Unavailable) | None => (None, None),
        }
    }
}

pub(super) fn command_replay(value: Json<'_>) -> Option<CommandReplay> {
    let mut fields = Fields::new(value)?;
    let replay = match fields.string("kind")?.as_str() {
        "unavailable" => CommandReplay::Unavailable,
        "available" => CommandReplay::Available {
            handle: durable_text(fields.required("handle")?)?,
            framed_bytes: fields.unsigned("framed_bytes")?,
        },
        _ => return None,
    };
    fields.finish(replay)
}

pub(super) fn cancelled_command(value: Json<'_>) -> Option<CancelledCommand> {
    let mut fields = Fields::new(value)?;
    let cancelled = CancelledCommand {
        replay: fields.present_or_null("output_replay", |value| command_replay(value).map(Some))?,
        artifact: fields.present_or_null("command_artifact_handle", |value| {
            durable_text(value).map(Some)
        })?,
    };
    fields.finish(cancelled)
}

pub(super) fn file_presentation(value: Json<'_>) -> Option<CommittedFilePresentation> {
    let mut fields = Fields::new(value)?;
    let presentation = CommittedFilePresentation {
        path: durable_text(fields.required("path")?)?,
        kind: tag(&fields.required("kind")?)?,
        lines: list(fields.required("lines")?, presentation_line)?,
        additions: fields.unsigned("additions")?,
        deletions: fields.unsigned("deletions")?,
        truncated: fields.flag("truncated")?,
        previous_content: fields
            .present_or_null("previous_content", |value| durable_text(value).map(Some))?,
        after_content: fields
            .present_or_null("after_content", |value| durable_text(value).map(Some))?,
        lifecycle_id: fields
            .present_or_null("lifecycle_id", |value| lifecycle_id(value).map(Some))?,
        content_handle: fields.nullable("content_handle", |value| durable_text(value).map(Some))?,
    };
    fields.finish(presentation)
}

fn presentation_line(value: Json<'_>) -> Option<PresentationLine> {
    let mut fields = Fields::new(value)?;
    let line = PresentationLine {
        kind: tag(&fields.required("kind")?)?,
        old_line: fields.present_or_null("old_line", |value| line_number(&value).map(Some))?,
        new_line: fields.present_or_null("new_line", |value| line_number(&value).map(Some))?,
        text: durable_text(fields.required("text")?)?,
    };
    fields.finish(line)
}

fn line_number(value: &Json<'_>) -> Option<u32> {
    u32::try_from(value.as_u64()?).ok()
}

fn lifecycle_id(value: Json<'_>) -> Option<LifecycleId> {
    let mut fields = Fields::new(value)?;
    let id = LifecycleId {
        turn_id: fields.unsigned("turn_id")?,
        call_id: durable_text(fields.required("call_id")?)?,
    };
    fields.finish(id)
}
