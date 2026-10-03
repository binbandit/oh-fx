use ofx_config::PrivateDir;
use ofx_contract::{RecoveredTurn, RecoveryPoint, StepResult};

use crate::result_store::{
    PREVIEW_BYTES, STORED_TEXT_MAX_BYTES, make_handle, preview, store_result,
};
use crate::session_codec::SavedProvider;
use crate::session_codec::recovery_checkpoint::{
    CheckpointSource, MAX_RECOVERY_FILE_BYTES, RecoveryCheckpoint, RouteCredential, SavedOutput,
    decode_recovery_file, encode_recovery_file,
};
use crate::session_error::SessionError;
use crate::session_event::{ConversationEvent, InterruptReason, InterruptedEvent};
use crate::session_log::conversation_progress::ProgressPoint;
use crate::session_log::conversation_writer::ConversationWriter;
use crate::session_log::managed_file::read_managed_file;
use crate::session_log::now_ms;
use crate::session_log::turn_events::{TurnArtifacts, saved_replay, turn_events};

const RECOVERY_FILE: &str = "recovery.json";
const RECOVERY_ASKED_FILE: &str = "recovery.asked";

#[derive(Debug, Default)]
pub(crate) enum Recovery {
    #[default]
    Absent,
    Pending(Box<RecoveryCheckpoint>),
    Continuing,
    Saved,
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

    pub fn authorizes(&self, credential: RouteCredential) -> bool {
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
    let artifacts = TurnArtifacts {
        dir,
        provider,
        timestamp_ms,
        earlier_files: &[],
    };
    let mut events = turn_events(&artifacts, &checkpoint.interrupted_turn(), written)?;
    if let Some(ConversationEvent::Interrupted(interrupted)) = events.last_mut() {
        interrupted.files = checkpoint.into_files();
    }
    writer.append(timestamp_ms, &events[usize::from(open)..])?;
    clear_recovery(dir);
    Ok(())
}

pub(crate) fn save_checkpoint(
    dir: &PrivateDir,
    conversation_seq: u64,
    point: &RecoveryPoint<'_>,
    provider: &SavedProvider,
    credential: RouteCredential,
) -> Result<(), SessionError> {
    let steps = &point.turn.steps;
    let source = CheckpointSource {
        point,
        provider,
        credential: Some(credential),
        replays: steps
            .iter()
            .map(|step| {
                step.provider_replay
                    .and_then(|replay| saved_replay(replay, provider))
            })
            .collect(),
        outputs: steps
            .iter()
            .map(|step| {
                step.tool_results
                    .iter()
                    .map(|result| spilled_output(dir, result))
                    .collect()
            })
            .collect(),
        created_at_ms: now_ms(),
    };
    let Some(bytes) = encode_recovery_file(conversation_seq, &source)? else {
        return Ok(());
    };
    dir.replace(RECOVERY_FILE, &bytes)?;
    let _ = dir.remove(RECOVERY_ASKED_FILE);
    Ok(())
}

fn spilled_output(dir: &PrivateDir, result: &StepResult<'_>) -> SavedOutput {
    let inline = SavedOutput {
        handle: None,
        preview: None,
    };
    let size = result.output.len();
    if size <= PREVIEW_BYTES || size > STORED_TEXT_MAX_BYTES {
        return inline;
    }
    let handle = make_handle(result.call_id, result.tool_name, result.output);
    if store_result(dir, &handle, result.output).is_err() {
        return inline;
    }
    SavedOutput {
        handle: Some(handle),
        preview: Some(preview(result.output).to_owned()),
    }
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
