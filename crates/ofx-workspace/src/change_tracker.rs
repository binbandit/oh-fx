use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::pathing::{FileIdentity, FileMutationTarget};

mod reversal;

const MAX_STACK_SIZE: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOperation {
    pub target: FileMutationTarget,
    pub anchor_identity: FileIdentity,
    pub parent_identities: Vec<FileIdentity>,
    pub previous_content: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoResult {
    Restored(PathBuf),
    Deleted(PathBuf),
    Unavailable(PathBuf),
    Empty,
}

#[derive(Debug, Clone, Default)]
pub struct ChangeTracker {
    stack: Arc<Mutex<VecDeque<FileOperation>>>,
}

impl ChangeTracker {
    pub fn push_operation(&self, operation: FileOperation) {
        let mut stack = self.stack();
        if stack.len() >= MAX_STACK_SIZE {
            stack.pop_front();
        }
        stack.push_back(operation);
    }

    pub fn clear(&self) {
        self.stack().clear();
    }

    pub fn undo_last(&self) -> UndoResult {
        let Some(operation) = self.stack().pop_back() else {
            return UndoResult::Empty;
        };
        let path = operation.target.path();
        match reversal::reverse(&operation) {
            Ok(reversal::Reversal::Restored) => UndoResult::Restored(path),
            Ok(reversal::Reversal::Deleted) => UndoResult::Deleted(path),
            Err(reversal::Unavailable) => UndoResult::Unavailable(path),
        }
    }

    fn stack(&self) -> MutexGuard<'_, VecDeque<FileOperation>> {
        self.stack.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests;
