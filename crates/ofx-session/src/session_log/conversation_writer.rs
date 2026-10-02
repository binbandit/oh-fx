use std::fs::File;
use std::os::unix::fs::FileExt;

use crate::session_error::SessionError;
use crate::session_event::{
    ConversationEvent, ConversationState, decode_conversation_frame, encode_conversation_frame,
};
use crate::session_log::conversation_history::ReplayScan;
use crate::session_log::conversation_progress::{ConversationProgress, ProgressPoint};
use crate::session_replay::{LineRead, LineReader};

pub(crate) struct ConversationWriter {
    file: File,
    committed_bytes: u64,
    state: ConversationState,
    failure: Option<SessionError>,
    #[cfg(test)]
    failing_syncs: usize,
}

pub(crate) struct LogScan {
    pub(crate) state: ConversationState,
    pub(crate) complete_bytes: u64,
    pub(crate) torn: bool,
    pub(crate) open_turn: Option<OpenTurn>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpenTurn {
    truncate_from: u64,
    prior_seq: u64,
    checkpointed: bool,
}

pub(crate) fn scan_log(
    file: &File,
    length: u64,
    replay: &mut ReplayScan,
) -> Result<LogScan, SessionError> {
    let mut state = ConversationState::default();
    let mut open_turn: Option<OpenTurn> = None;
    let mut reader = LineReader::new(file, 0, length)?;
    let mut torn = false;
    loop {
        let offset = reader.offset();
        let line = match reader.next_line()? {
            LineRead::Line(line) => line,
            LineRead::End => break,
            LineRead::Torn => {
                torn = true;
                break;
            }
        };
        let envelope = decode_conversation_frame(&line)?;
        let seq = envelope.seq;
        state.apply(seq, envelope.timestamp_ms(), &envelope.event)?;
        match &envelope.event {
            ConversationEvent::User(_) => {
                open_turn = Some(OpenTurn {
                    truncate_from: offset,
                    prior_seq: seq - 1,
                    checkpointed: false,
                });
            }
            ConversationEvent::TurnCompleted(_) | ConversationEvent::Interrupted(_) => {
                open_turn = None;
            }
            ConversationEvent::ContextCheckpoint(_) => {
                if let Some(turn) = open_turn.as_mut() {
                    *turn = OpenTurn {
                        truncate_from: reader.offset(),
                        prior_seq: seq,
                        checkpointed: true,
                    };
                }
            }
            ConversationEvent::Assistant(_)
            | ConversationEvent::ToolCall(_)
            | ConversationEvent::ToolResult(_)
            | ConversationEvent::Steering(_) => {}
        }
        replay.observe(offset, seq, &envelope.event)?;
    }
    Ok(LogScan {
        state,
        complete_bytes: reader.offset(),
        torn,
        open_turn,
    })
}

fn closes_commit_unit(event: &ConversationEvent) -> bool {
    matches!(
        event,
        ConversationEvent::TurnCompleted(_)
            | ConversationEvent::Interrupted(_)
            | ConversationEvent::ContextCheckpoint(_)
    )
}

impl ConversationWriter {
    pub(crate) fn new(file: File) -> Self {
        Self {
            file,
            committed_bytes: 0,
            state: ConversationState::default(),
            failure: None,
            #[cfg(test)]
            failing_syncs: 0,
        }
    }

    pub(crate) fn open(file: File, replay: &mut ReplayScan) -> Result<Self, SessionError> {
        let length = file.metadata()?.len();
        let scan = scan_log(&file, length, replay)?;
        let mut writer = Self::new(file);
        writer.state = scan.state;
        writer.committed_bytes = scan.complete_bytes;
        if scan.torn {
            writer.cut(scan.complete_bytes)?;
        }
        if let Some(turn) = scan.open_turn {
            writer.cut(turn.truncate_from)?;
            writer
                .state
                .rewind_open_turn(turn.prior_seq, turn.checkpointed);
            replay.rewind(turn.prior_seq, turn.checkpointed);
        }
        Ok(writer)
    }

    pub(crate) fn file(&self) -> &File {
        &self.file
    }

    pub(crate) fn committed_bytes(&self) -> u64 {
        self.committed_bytes
    }

    pub(crate) fn last_seq(&self) -> u64 {
        self.state.last_seq()
    }

    pub(crate) fn turn_open(&self) -> bool {
        self.state.turn_open()
    }

    pub(crate) fn context_progress(
        &self,
        cut: Option<ProgressPoint>,
    ) -> Result<ConversationProgress, SessionError> {
        let coverage = self.state.latest_checkpoint_coverage();
        let mut progress = ConversationProgress::from_coverage(coverage);
        let mut reader = LineReader::new(&self.file, 0, self.committed_bytes)?;
        while let LineRead::Line(line) = reader.next_line()? {
            let envelope = decode_conversation_frame(&line)?;
            if envelope.seq > coverage {
                progress.observe(envelope.seq, &envelope.event, cut)?;
            }
        }
        Ok(progress)
    }

    pub(crate) fn context_coverage(
        &self,
        cut: ProgressPoint,
        upcoming: &[ConversationEvent],
    ) -> Result<u64, SessionError> {
        let mut progress = self.context_progress(Some(cut))?;
        let mut seq = self.last_seq();
        for event in upcoming {
            seq = seq
                .checked_add(1)
                .ok_or(SessionError::ConversationSequenceOverflow)?;
            progress.observe(seq, event, Some(cut))?;
        }
        if !progress.reached && cut != ProgressPoint::default() {
            return Err(SessionError::InvalidContextHistoryStart);
        }
        Ok(progress.coverage)
    }

    pub(crate) fn failure(&self) -> Option<SessionError> {
        self.failure
    }

    pub(crate) fn mark_uncertain(&mut self) {
        self.failure = Some(SessionError::SessionPersistenceUncertain);
    }

    pub(crate) fn block_open_turn(&mut self) {
        self.failure
            .get_or_insert(SessionError::SessionCommitFailed);
    }

    pub(crate) fn append(
        &mut self,
        timestamp_ms: i64,
        events: &[ConversationEvent],
    ) -> Result<(), SessionError> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        let Some((_, earlier)) = events.split_last() else {
            return Ok(());
        };
        if earlier.iter().any(closes_commit_unit) {
            return Err(SessionError::InvalidConversationEvent);
        }
        if self.state.has_pending_tool_calls() {
            return Err(SessionError::UnresolvedToolCall);
        }
        let mut next = self.state.clone();
        let mut bytes = Vec::new();
        for event in events {
            let seq = next.next_seq()?;
            next.apply(seq, timestamp_ms, event)?;
            bytes.extend(encode_conversation_frame(seq, timestamp_ms, event)?);
        }
        if next.has_pending_tool_calls() {
            return Err(SessionError::UnresolvedToolCall);
        }
        self.write_prepared(&bytes)?;
        self.state = next;
        Ok(())
    }

    fn write_prepared(&mut self, bytes: &[u8]) -> Result<(), SessionError> {
        let next_bytes = u64::try_from(bytes.len())
            .ok()
            .and_then(|length| self.committed_bytes.checked_add(length))
            .ok_or(SessionError::EventFrameTooLarge)?;
        if self.file.metadata()?.len() != self.committed_bytes {
            self.failure = Some(SessionError::SessionWriterChanged);
            return Err(SessionError::SessionWriterChanged);
        }
        if let Err(error) = self.file.write_all_at(bytes, self.committed_bytes) {
            return Err(self.roll_back(error.into()));
        }
        if let Err(error) = self.sync() {
            return Err(self.roll_back(error));
        }
        self.committed_bytes = next_bytes;
        Ok(())
    }

    fn roll_back(&mut self, original: SessionError) -> SessionError {
        self.failure = Some(SessionError::SessionPersistenceUncertain);
        if self.file.set_len(self.committed_bytes).is_err() || self.sync().is_err() {
            return SessionError::SessionPersistenceUncertain;
        }
        self.failure = None;
        original
    }

    fn cut(&mut self, length: u64) -> Result<(), SessionError> {
        self.file.set_len(length)?;
        self.sync()?;
        self.committed_bytes = length;
        Ok(())
    }

    fn sync(&mut self) -> Result<(), SessionError> {
        #[cfg(test)]
        if self.failing_syncs > 0 {
            self.failing_syncs -= 1;
            return Err(SessionError::Io(std::io::ErrorKind::Other));
        }
        self.file.sync_all().map_err(SessionError::from)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_syncs(&mut self, count: usize) {
        self.failing_syncs = count;
    }
}
