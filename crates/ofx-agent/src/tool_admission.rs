use std::mem;

use ofx_contract::{ToolCall, ToolResultStatus};
use sha2::{Digest, Sha256};

const SHELL_TOOL: &str = "shell";
const SHELL_EXECUTION_FAILURE_DOMAIN: &[u8] = b"fx.shell-execution-failure.v1\0";

type BatchDigest = [u8; 32];

#[derive(Debug, Default)]
struct ConsecutiveBatchRepeats {
    previous: Vec<BatchDigest>,
    current: Vec<BatchDigest>,
    stop_after_batch: bool,
}

impl ConsecutiveBatchRepeats {
    fn begin_batch(&mut self) {
        self.current.clear();
        self.stop_after_batch = false;
    }

    fn observe(&mut self, digest: BatchDigest) {
        if !self.current.contains(&digest) {
            self.current.push(digest);
        }
        self.stop_after_batch |= self.previous.contains(&digest);
    }

    fn finish_batch(&mut self) -> bool {
        if self.stop_after_batch {
            return true;
        }
        mem::swap(&mut self.previous, &mut self.current);
        self.current.clear();
        false
    }
}

#[derive(Debug, Default)]
pub(crate) struct ShellExecutionFailureRetry(ConsecutiveBatchRepeats);

impl ShellExecutionFailureRetry {
    pub(crate) fn begin_batch(&mut self) {
        self.0.begin_batch();
    }

    pub(crate) fn observe(&mut self, call: &ToolCall, status: ToolResultStatus) {
        if call.name != SHELL_TOOL || status != ToolResultStatus::Failure {
            return;
        }
        let mut hash = Sha256::new();
        hash.update(SHELL_EXECUTION_FAILURE_DOMAIN);
        hash.update(call.arguments.as_bytes());
        self.0.observe(hash.finalize().into());
    }

    pub(crate) fn finish_batch(&mut self) -> bool {
        self.0.finish_batch()
    }
}

#[cfg(test)]
mod tests;
