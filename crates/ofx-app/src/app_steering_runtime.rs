use std::sync::Arc;

use ofx_agent::WorkerRuntime;
use ofx_tui::SteeringQueue;

pub(crate) struct WaitingSteering(pub(crate) Arc<WorkerRuntime>);

impl SteeringQueue for WaitingSteering {
    fn retract_newest(&self) -> Option<(u64, String)> {
        self.0
            .pop_queued_steer_for_edit()
            .map(|prompt| (prompt.id, prompt.text))
    }
}
