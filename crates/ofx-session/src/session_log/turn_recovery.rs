use ofx_config::PrivateDir;
use ofx_contract::RecoveredTurn;

use crate::session_codec::SavedProvider;
use crate::session_codec::recovery_checkpoint::{
    CredentialAuthority, MAX_RECOVERY_FILE_BYTES, RecoveryCheckpoint, decode_recovery_file,
};
use crate::session_error::SessionError;
use crate::session_event::{ConversationEvent, InterruptReason, InterruptedEvent, KeptReplay};
use crate::session_log::conversation_progress::ProgressPoint;
use crate::session_log::conversation_writer::ConversationWriter;
use crate::session_log::managed_file::read_managed_file;
use crate::session_log::now_ms;
use crate::session_log::turn_events::{TurnArtifacts, turn_events};

const RECOVERY_FILE: &str = "recovery.json";
const RECOVERY_ASKED_FILE: &str = "recovery.asked";

#[derive(Debug, Default)]
pub(crate) enum Recovery {
    #[default]
    Absent,
    Pending(Box<RecoveryCheckpoint>),
    Continuing(Vec<KeptReplay>),
}

#[derive(Debug)]
pub struct PendingRecovery(Box<RecoveryCheckpoint>);

impl PendingRecovery {
    pub(crate) fn new(mut checkpoint: Box<RecoveryCheckpoint>, dir: &PrivateDir) -> Self {
        checkpoint.restore_outputs(dir);
        Self(checkpoint)
    }

    pub fn prompt(&self) -> &str {
        self.0.prompt()
    }

    pub fn authorizes(&self, credential: CredentialAuthority<'_>) -> bool {
        self.0.authorizes(credential)
    }

    pub fn into_turn(
        self,
        provider: &SavedProvider,
        model: &str,
        fast_mode: bool,
    ) -> RecoveredTurn {
        self.0.into_continuation(provider, model, fast_mode)
    }
}

pub(crate) fn open_unfinished_turn(
    dir: &PrivateDir,
    writer: &mut ConversationWriter,
) -> Result<Recovery, SessionError> {
    if let Some(checkpoint) = read_checkpoint(dir, writer.last_seq())? {
        return Ok(Recovery::Pending(Box::new(checkpoint)));
    }
    if writer.turn_open() {
        let interrupted =
            ConversationEvent::Interrupted(InterruptedEvent::new(InterruptReason::Failed, None));
        writer.append(now_ms(), &[interrupted])?;
    }
    Ok(Recovery::Absent)
}

pub(crate) fn commit_checkpoint(
    dir: &PrivateDir,
    writer: &mut ConversationWriter,
    provider: &SavedProvider,
    mut checkpoint: Box<RecoveryCheckpoint>,
) -> Result<(), SessionError> {
    let timestamp_ms = now_ms();
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
    clear_recovery(dir);
    Ok(())
}

pub(crate) fn clear_recovery(dir: &PrivateDir) {
    let _ = dir.remove(RECOVERY_FILE);
    let _ = dir.remove(RECOVERY_ASKED_FILE);
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
