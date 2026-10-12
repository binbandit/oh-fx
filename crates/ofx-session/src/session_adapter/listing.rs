use ofx_trace::trace_log;

use crate::session_codec::{DEFAULT_CONVERSATION_LANGUAGE, is_valid_conversation_language};
use crate::session_error::SessionError;
use crate::session_manager::{IoFault, ListCursor, Manager, Summary, full_string};
use crate::session_summary_codec::{SessionSource, SessionSummary};
use crate::session_trace::SESSION;

pub(super) const LIST_PAGE_SIZE: usize = 256;

pub(super) enum RecordError {
    UnexpectedToken,
    InvalidDurableField,
}

impl RecordError {
    fn name(&self) -> &'static str {
        match self {
            Self::UnexpectedToken => "UnexpectedToken",
            Self::InvalidDurableField => "InvalidDurableField",
        }
    }
}

pub(super) fn list_items(manager: &Manager) -> Result<Vec<Summary>, IoFault> {
    let mut items = Vec::new();
    let mut cursor: Option<ListCursor> = None;
    loop {
        let page = manager.list(cursor.as_ref(), LIST_PAGE_SIZE)?;
        items.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(items),
        }
    }
}

pub(super) fn summary_of(
    item: &Summary,
    source: SessionSource,
) -> Result<SessionSummary, RecordError> {
    let title = item
        .title
        .as_deref()
        .map(|raw| full_string(raw).ok_or(RecordError::UnexpectedToken))
        .transpose()?;
    let conversation_language = match item.language.as_deref() {
        Some(raw) => {
            let tag = full_string(raw).ok_or(RecordError::UnexpectedToken)?;
            if !is_valid_conversation_language(&tag) {
                return Err(RecordError::InvalidDurableField);
            }
            tag
        }
        None => DEFAULT_CONVERSATION_LANGUAGE.to_owned(),
    };
    Ok(SessionSummary {
        id: item.id.clone(),
        workspace_root: item.workspace.clone(),
        origin_workspace_root: item.workspace.clone(),
        title,
        created_at_ms: i64::try_from(item.created_ms).unwrap_or(0),
        updated_at_ms: i64::try_from(item.updated_ms).unwrap_or(0),
        conversation_language,
        history_len: usize::try_from(item.turns.max(1)).unwrap_or(usize::MAX),
        has_checkpoint: false,
        source,
    })
}

pub(super) fn store_error(fault: IoFault) -> SessionError {
    trace_log!(
        SESSION,
        "event=sessions_v2_command_failed kind=store err={fault}"
    );
    SessionError::SessionStoreUnavailable
}

pub(super) fn record_error(error: &RecordError) -> SessionError {
    trace_log!(
        SESSION,
        "event=sessions_v2_command_failed kind=record err={}",
        error.name()
    );
    SessionError::InvalidSessionFormat
}
