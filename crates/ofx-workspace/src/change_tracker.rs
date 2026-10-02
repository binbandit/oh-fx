use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_STACK_SIZE: usize = 100;
const DEFAULT_FILE_MODE: u32 = 0o666;
const PERMISSION_BITS: u32 = 0o7777;
const WRITE_BITS: u32 = 0o222;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOperation {
    pub path: PathBuf,
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
        match operation.previous_content {
            Some(content) => match restore_content(&operation.path, &content) {
                Ok(()) => UndoResult::Restored(operation.path),
                Err(_) => UndoResult::Unavailable(operation.path),
            },
            None => match fs::remove_file(&operation.path) {
                Err(error) if error.kind() != ErrorKind::NotFound => {
                    UndoResult::Unavailable(operation.path)
                }
                _ => UndoResult::Deleted(operation.path),
            },
        }
    }

    fn stack(&self) -> MutexGuard<'_, VecDeque<FileOperation>> {
        self.stack.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn restore_content(path: &Path, content: &[u8]) -> io::Result<()> {
    let existing_mode = fs::metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
        .map(|metadata| metadata.permissions().mode() & PERMISSION_BITS);
    if existing_mode.is_some_and(|mode| mode & WRITE_BITS == 0) {
        return Err(ErrorKind::PermissionDenied.into());
    }
    let mut temp_path = path.as_os_str().to_owned();
    temp_path.push(format!(".tmp.{}", nano_timestamp()));
    let temp_path = PathBuf::from(temp_path);
    let replaced = write_and_rename(
        &temp_path,
        path,
        content,
        existing_mode.unwrap_or(DEFAULT_FILE_MODE),
    );
    if replaced.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    replaced
}

fn write_and_rename(temp_path: &Path, path: &Path, content: &[u8], mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(temp_path)?;
    file.write_all(content)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp_path, path)
}

fn nano_timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

#[cfg(test)]
mod tests;
