use ofx_trace::trace_log;

use super::WritableSession;
use crate::session_error::SessionError;
use crate::session_store::RememberedPointer;
use crate::session_trace::SESSION;

pub(crate) enum Remembering {
    OnFirstWrite(RememberedPointer),
    Failed(SessionError),
}

impl WritableSession {
    pub(crate) fn remember_on_first_write(&mut self, pointer: RememberedPointer) {
        self.remembering = Some(Box::new(Remembering::OnFirstWrite(pointer)));
    }

    pub(crate) fn remember_after_write(&mut self) {
        let Some(Remembering::OnFirstWrite(pointer)) = self.remembering.as_deref() else {
            return;
        };
        if !self.has_durable_work() {
            return;
        }
        let failure = pointer.remember(&self.metadata.id).err();
        if let Some(error) = failure {
            trace_log!(
                SESSION,
                "event=remember_session_failed id={} err={error}",
                self.metadata.id
            );
        }
        self.remembering = failure.map(|error| Box::new(Remembering::Failed(error)));
    }

    pub fn take_remember_failure(&mut self) -> Option<SessionError> {
        let Some(Remembering::Failed(error)) = self.remembering.as_deref() else {
            return None;
        };
        let error = *error;
        self.remembering = None;
        Some(error)
    }
}
