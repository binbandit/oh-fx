use std::path::Path;
use std::process::Command;
use std::sync::{PoisonError, RwLock, RwLockWriteGuard};

static SPAWNS: RwLock<()> = RwLock::new(());

pub(crate) fn make_fifo(path: &Path) -> bool {
    let _spawning = SPAWNS.read().unwrap_or_else(PoisonError::into_inner);
    Command::new("mkfifo").arg(path).status().unwrap().success()
}

pub(crate) fn hold_off_spawns() -> RwLockWriteGuard<'static, ()> {
    SPAWNS.write().unwrap_or_else(PoisonError::into_inner)
}
