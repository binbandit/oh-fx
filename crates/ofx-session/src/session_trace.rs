use ofx_trace::trace_log;

use crate::session_error::SessionError;

pub(crate) const SESSION: &str = "session";

pub(crate) fn rejected(rule: &'static str) -> SessionError {
    trace_log!(SESSION, "conversation event rejected rule={rule}");
    SessionError::InvalidConversationEvent
}
