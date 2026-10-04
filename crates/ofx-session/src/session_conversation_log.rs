use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_contract::{ConversationLog, HistoryCut, HistoryTurn, LogFailure, RecoveryPoint};

use crate::session_codec::SavedProvider;
use crate::session_codec::recovery_checkpoint::RouteCredential;
use crate::session_error::SessionError;
use crate::session_log::WritableSession;

pub struct SessionLog {
    session: Arc<Mutex<WritableSession>>,
    provider: SavedProvider,
    credential: RouteCredential,
}

impl SessionLog {
    pub fn new(
        session: Arc<Mutex<WritableSession>>,
        provider: SavedProvider,
        credential: RouteCredential,
    ) -> Self {
        Self {
            session,
            provider,
            credential,
        }
    }

    fn session(&self) -> MutexGuard<'_, WritableSession> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ConversationLog for SessionLog {
    fn require_writable(&self) -> Result<(), LogFailure> {
        self.session().require_writable().map_err(log_failure)
    }

    fn record_turn(&mut self, turn: &HistoryTurn<'_>) -> Result<(), LogFailure> {
        self.session()
            .record_turn(turn, &self.provider)
            .map_err(log_failure)
    }

    fn record_compaction(
        &mut self,
        checkpoint: &str,
        cut: HistoryCut,
        active: Option<&HistoryTurn<'_>>,
    ) -> Result<(), LogFailure> {
        self.session()
            .record_compaction(checkpoint, cut, active, &self.provider)
            .map_err(log_failure)
    }

    fn record_recovery(&self, point: &RecoveryPoint<'_>) -> Result<(), LogFailure> {
        self.session()
            .record_recovery(point, &self.provider, self.credential)
            .map_err(log_failure)
    }

    fn clear_recovery(&self) -> Result<(), LogFailure> {
        self.session().discard_recovery();
        Ok(())
    }
}

fn log_failure(error: SessionError) -> LogFailure {
    LogFailure {
        code: error.to_string(),
    }
}
