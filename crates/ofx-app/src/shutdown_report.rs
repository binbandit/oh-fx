use std::fmt::Write as _;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use ofx_config::{DurableError, PrivateDir};
use ofx_trace::trace_log;

const SHUTDOWN: &str = "shutdown";
const DIRECTORY: &str = "diagnostics";
const FILE: &str = "last-shutdown.json";
const MAX_STAGES: usize = 16;
const MAX_REPORT_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Default)]
pub(crate) struct ShutdownTrace(Arc<Mutex<Option<Stages>>>);

#[derive(Debug)]
struct Stages {
    started: Instant,
    last: Instant,
    recorded: Vec<Stage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stage {
    name: &'static str,
    step_ms: u128,
    total_ms: u128,
}

impl ShutdownTrace {
    pub(crate) fn mark(&self, name: &'static str) {
        let now = Instant::now();
        let mut stages = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let stages = stages.get_or_insert_with(|| Stages {
            started: now,
            last: now,
            recorded: Vec::with_capacity(MAX_STAGES),
        });
        let step_ms = now.duration_since(stages.last).as_millis();
        let total_ms = now.duration_since(stages.started).as_millis();
        trace_log!(
            SHUTDOWN,
            "stage name={name} step_ms={step_ms} total_ms={total_ms}"
        );
        stages.last = now;
        if stages.recorded.len() < MAX_STAGES {
            stages.recorded.push(Stage {
                name,
                step_ms,
                total_ms,
            });
        }
    }

    pub(crate) fn save(&self, state: &Path, recorded_at_ms: i64) {
        let report = self.report(recorded_at_ms);
        if let Err(error) = replace(state, &report) {
            trace_log!(SHUTDOWN, "last shutdown report write failed err={error}");
        }
    }

    fn report(&self, recorded_at_ms: i64) -> String {
        let stages = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (total_ms, recorded) = stages.as_ref().map_or((0, &[][..]), |stages| {
            (
                stages.last.duration_since(stages.started).as_millis(),
                stages.recorded.as_slice(),
            )
        });
        let mut report = format!(
            "{{\"version\":1,\"recorded_at_ms\":{recorded_at_ms},\"total_ms\":{total_ms},\"stages\":["
        );
        for (index, stage) in recorded.iter().enumerate() {
            if index > 0 {
                report.push(',');
            }
            let _ = write!(
                report,
                "{{\"name\":\"{}\",\"step_ms\":{},\"total_ms\":{}}}",
                stage.name, stage.step_ms, stage.total_ms
            );
        }
        report.push_str("]}");
        report
    }
}

fn replace(state: &Path, report: &str) -> Result<(), DurableError> {
    PrivateDir::open_or_create(state)?
        .open_or_create_child(DIRECTORY)?
        .replace(FILE, report.as_bytes())
}

pub(crate) fn read_last(state: &Path) -> Option<Vec<u8>> {
    let file = File::open(state.join(DIRECTORY).join(FILE)).ok()?;
    let mut contents = Vec::new();
    file.take(MAX_REPORT_BYTES + 1)
        .read_to_end(&mut contents)
        .ok()?;
    u64::try_from(contents.len())
        .is_ok_and(|bytes| bytes <= MAX_REPORT_BYTES)
        .then_some(contents)
}

#[cfg(test)]
mod tests;
