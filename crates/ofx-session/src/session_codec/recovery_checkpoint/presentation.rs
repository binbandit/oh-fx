use ofx_contract::{
    CommandOutputReplay, CommittedFilePresentation, FilePresentationLine, ToolLifecycleId,
};
use serde::Serialize;

use super::durable::durable_text;
use super::{list, tag};
use crate::json_fields::{Fields, Json};
use crate::session_event::{WireTag, has_one_content_source};

const AVAILABLE: &str = "available";
const UNAVAILABLE: &str = "unavailable";

#[derive(Serialize)]
pub(super) struct FilePresentationWire<'a> {
    path: &'a str,
    kind: &'static str,
    lines: Vec<LineWire<'a>>,
    additions: u64,
    deletions: u64,
    truncated: bool,
    previous_content: Option<&'a str>,
    after_content: Option<&'a str>,
    lifecycle_id: Option<LifecycleWire<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_handle: Option<&'a str>,
}

#[derive(Serialize)]
struct LineWire<'a> {
    kind: &'static str,
    old_line: Option<u32>,
    new_line: Option<u32>,
    text: &'a str,
}

#[derive(Serialize)]
struct LifecycleWire<'a> {
    turn_id: u64,
    call_id: &'a str,
}

impl<'a> From<&'a CommittedFilePresentation> for FilePresentationWire<'a> {
    fn from(presentation: &'a CommittedFilePresentation) -> Self {
        Self {
            path: &presentation.path,
            kind: presentation.kind.tag(),
            lines: presentation
                .lines
                .iter()
                .map(|line| LineWire {
                    kind: line.kind.tag(),
                    old_line: line.old_line,
                    new_line: line.new_line,
                    text: &line.text,
                })
                .collect(),
            additions: presentation.additions,
            deletions: presentation.deletions,
            truncated: presentation.truncated,
            previous_content: presentation.previous_content.as_deref(),
            after_content: presentation.after_content.as_deref(),
            lifecycle_id: presentation.lifecycle_id.as_ref().map(|id| LifecycleWire {
                turn_id: id.turn_id,
                call_id: &id.call_id,
            }),
            content_handle: presentation.content_handle.as_deref(),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum ReplayWire<'a> {
    Available { handle: &'a str, framed_bytes: u64 },
    Unavailable,
}

impl<'a> From<&'a CommandOutputReplay> for ReplayWire<'a> {
    fn from(replay: &'a CommandOutputReplay) -> Self {
        match replay {
            CommandOutputReplay::Available {
                handle,
                framed_bytes,
            } => Self::Available {
                handle,
                framed_bytes: *framed_bytes,
            },
            CommandOutputReplay::Unavailable => Self::Unavailable,
        }
    }
}

pub(super) fn read_file_presentation(value: Json<'_>) -> Option<CommittedFilePresentation> {
    let mut fields = Fields::new(value)?;
    let presentation = CommittedFilePresentation {
        path: durable_text(fields.required("path")?)?,
        kind: tag(&fields.required("kind")?)?,
        lines: list(fields.required("lines")?, line)?,
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
    let inline = presentation.previous_content.is_some() || presentation.after_content.is_some();
    has_one_content_source(presentation.content_handle.as_deref(), inline).then_some(())?;
    fields.finish(presentation)
}

fn line(value: Json<'_>) -> Option<FilePresentationLine> {
    let mut fields = Fields::new(value)?;
    let line = FilePresentationLine {
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

fn lifecycle_id(value: Json<'_>) -> Option<ToolLifecycleId> {
    let mut fields = Fields::new(value)?;
    let id = ToolLifecycleId {
        turn_id: fields.unsigned("turn_id")?,
        call_id: durable_text(fields.required("call_id")?)?,
    };
    fields.finish(id)
}

pub(super) fn read_command_output_replay(value: Json<'_>) -> Option<CommandOutputReplay> {
    let mut fields = Fields::new(value)?;
    let replay = match fields.text("kind")?.as_ref() {
        UNAVAILABLE => CommandOutputReplay::Unavailable,
        AVAILABLE => CommandOutputReplay::Available {
            handle: durable_text(fields.required("handle")?)?,
            framed_bytes: fields.unsigned("framed_bytes")?,
        },
        _ => return None,
    };
    fields.finish(replay)
}
