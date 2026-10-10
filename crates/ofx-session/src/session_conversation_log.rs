use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use ofx_contract::{
    ConversationLog, DeliveryOutcome, FileChangeStats, HistoryCut, HistoryTurn, LogFailure,
    RecoveryPoint, RequestTicket,
};

use crate::session_codec::SavedProvider;
use crate::session_codec::recovery_checkpoint::RouteCredential;
use crate::session_error::SessionError;
use crate::session_log::WritableSession;

pub struct SessionLog {
    session: Arc<Mutex<WritableSession>>,
    usage: Weak<Mutex<WritableSession>>,
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
            usage: Arc::downgrade(&session),
            session,
            provider,
            credential,
        }
    }

    #[must_use]
    pub fn accounting_in(mut self, owner: Weak<Mutex<WritableSession>>) -> Self {
        self.usage = owner;
        self
    }

    fn session(&self) -> MutexGuard<'_, WritableSession> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn with_usage<T>(&self, account: impl FnOnce(&mut WritableSession) -> T) -> T {
        let owner = self
            .usage
            .upgrade()
            .unwrap_or_else(|| Arc::clone(&self.session));
        let mut session = owner.lock().unwrap_or_else(PoisonError::into_inner);
        account(&mut session)
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

    fn begin_request(&self) -> Result<RequestTicket, LogFailure> {
        self.with_usage(WritableSession::begin_request)
            .map_err(log_failure)
    }

    fn finish_request(
        &self,
        ticket: RequestTicket,
        outcome: DeliveryOutcome,
    ) -> Result<(), LogFailure> {
        self.with_usage(|session| session.finish_request(ticket, outcome))
            .map_err(log_failure)
    }

    fn record_committed_lines(&self, change: FileChangeStats) {
        self.with_usage(|session| session.record_committed_lines(change));
    }
}

fn log_failure(error: SessionError) -> LogFailure {
    LogFailure {
        code: error.to_string(),
    }
}
