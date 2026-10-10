use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Default)]
pub struct RecoveryPause(Arc<AtomicBool>);

impl RecoveryPause {
    pub fn request(&self) {
        ofx_trace::trace_event!(
            "recovery",
            "pause_requested",
            ofx_trace::TraceContext::default(),
            "source=interactive_try_later"
        );
        self.0.store(true, Ordering::Release);
    }

    pub(crate) fn requested(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn reset(&self) {
        self.0.store(false, Ordering::Release);
    }
}
