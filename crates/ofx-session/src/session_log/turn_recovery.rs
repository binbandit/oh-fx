use ofx_config::PrivateDir;

use crate::session_codec::SavedProvider;
use crate::session_codec::recovery_checkpoint::{
    MAX_RECOVERY_FILE_BYTES, RecoveryCheckpoint, decode_recovery_file,
};
use crate::session_error::SessionError;
use crate::session_event::{ConversationEvent, InterruptReason, InterruptedEvent};
use crate::session_log::conversation_progress::ProgressPoint;
use crate::session_log::conversation_writer::ConversationWriter;
use crate::session_log::managed_file::read_managed_file;
use crate::session_log::now_ms;
use crate::session_log::turn_events::{TurnArtifacts, turn_events};

const RECOVERY_FILE: &str = "recovery.json";
const RECOVERY_ASKED_FILE: &str = "recovery.asked";

pub(crate) fn close_unfinished_turn(
    dir: &PrivateDir,
    writer: &mut ConversationWriter,
    provider: &SavedProvider,
) -> Result<(), SessionError> {
    let timestamp_ms = now_ms();
    let Some(mut checkpoint) = read_checkpoint(dir, writer.last_seq())? else {
        if !writer.turn_open() {
            return Ok(());
        }
        let interrupted =
            ConversationEvent::Interrupted(InterruptedEvent::new(InterruptReason::Failed, None));
        return writer.append(timestamp_ms, &[interrupted]);
    };
    checkpoint.restore_outputs(dir);
    let open = writer.turn_open();
    let written = if open {
        writer.context_progress(None)?.point
    } else {
        ProgressPoint::default()
    };
    let saved_replays = checkpoint.saved_replays();
    let artifacts = TurnArtifacts {
        dir,
        provider,
        timestamp_ms,
        saved_replays: &saved_replays,
    };
    let mut events = turn_events(&artifacts, &checkpoint.interrupted_turn(), written)?;
    if let Some(ConversationEvent::Interrupted(interrupted)) = events.last_mut() {
        interrupted.files = checkpoint.into_files();
    }
    writer.append(timestamp_ms, &events[usize::from(open)..])?;
    let _ = dir.remove(RECOVERY_FILE);
    let _ = dir.remove(RECOVERY_ASKED_FILE);
    Ok(())
}

fn read_checkpoint(
    dir: &PrivateDir,
    conversation_seq: u64,
) -> Result<Option<RecoveryCheckpoint>, SessionError> {
    let bytes = read_managed_file(dir, RECOVERY_FILE, MAX_RECOVERY_FILE_BYTES).map_err(
        |error| match error {
            SessionError::InvalidSessionFormat => SessionError::InvalidRecoveryCheckpoint,
            other => other,
        },
    )?;
    bytes.map_or(Ok(None), |bytes| {
        decode_recovery_file(&bytes, conversation_seq)
    })
}

#[cfg(test)]
mod tests;
