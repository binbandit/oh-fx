#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use super::{
    Backend, Claim, Entry, EntryState, ExecutionError, MAX_LIVE_ENTRIES, ManagedExecutions, Phase,
    Registry, Shared, Snapshot, SnapshotState, evict_oldest_tombstone,
};

#[derive(Debug, Clone, Copy)]
pub struct TtyCommand<'a> {
    pub command: &'a str,
    pub cwd: &'a Path,
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct TtyUpdate<'a> {
    pub execution_id: &'a str,
    pub state: SnapshotState,
    pub output: &'a [u8],
    pub raw_bytes: usize,
    pub output_incomplete: bool,
    pub error_name: Option<&'static str>,
    pub published_running: bool,
}

pub struct TtyCapacity {
    shared: Arc<Shared>,
    held: bool,
}

impl TtyCapacity {
    fn release_in(mut self, registry: &mut Registry) {
        registry.pending_admissions = registry.pending_admissions.saturating_sub(1);
        self.held = false;
    }
}

impl Drop for TtyCapacity {
    fn drop(&mut self) {
        if self.held {
            let mut registry = self.shared.registry();
            registry.pending_admissions = registry.pending_admissions.saturating_sub(1);
        }
    }
}

impl ManagedExecutions {
    pub fn reserve_tty_capacity(&self) -> Result<TtyCapacity, ExecutionError> {
        let mut registry = self.shared.registry();
        if registry.stopping {
            return Err(ExecutionError::RuntimeStopping);
        }
        if registry.live_count() >= MAX_LIVE_ENTRIES {
            return Err(ExecutionError::ExecutionCapacityExceeded);
        }
        registry.pending_admissions += 1;
        Ok(TtyCapacity {
            shared: Arc::clone(&self.shared),
            held: true,
        })
    }

    pub fn register_tty(
        &self,
        capacity: Option<TtyCapacity>,
        command: &TtyCommand<'_>,
        update: &TtyUpdate<'_>,
    ) -> Result<Snapshot, ExecutionError> {
        let entry = self.insert_tty(capacity, command, update)?;
        Ok(self.deliver(&entry, false))
    }

    pub fn index_tty(
        &self,
        capacity: Option<TtyCapacity>,
        command: &TtyCommand<'_>,
        update: &TtyUpdate<'_>,
    ) -> Result<(), ExecutionError> {
        self.insert_tty(capacity, command, update).map(drop)
    }

    pub fn update_tty(&self, update: &TtyUpdate<'_>) -> Result<Snapshot, ExecutionError> {
        let entry = self.live_tty(update.execution_id)?;
        apply(&mut entry.state(), update);
        Ok(self.deliver(&entry, false))
    }

    pub fn refresh_tty(&self, update: &TtyUpdate<'_>) -> Result<(), ExecutionError> {
        let entry = self.live_tty(update.execution_id)?;
        apply(&mut entry.state(), update);
        Ok(())
    }

    pub fn observe_tty_state(&self, execution_id: &str, state: SnapshotState) {
        if let Ok(entry) = self.live_tty(execution_id) {
            entry.state().phase = Phase::from_snapshot_state(state);
        }
    }

    pub fn backend_for(&self, execution_id: &str) -> Option<Backend> {
        self.find(execution_id).ok().map(|entry| entry.backend)
    }

    pub fn state_for(&self, execution_id: &str) -> Option<SnapshotState> {
        self.find(execution_id)
            .ok()
            .map(|entry| entry.state().phase.snapshot_state())
    }

    pub fn is_tombstone(&self, execution_id: &str) -> bool {
        self.find(execution_id)
            .is_ok_and(|entry| entry.state().tombstone.is_some())
    }

    pub fn reserve_external_wait(&self, execution_id: &str) -> Result<u64, ExecutionError> {
        let entry = self.find(execution_id)?;
        let waiter = {
            let mut registry = self.shared.registry();
            registry.next_waiter = registry.next_waiter.wrapping_add(1).max(1);
            registry.next_waiter
        };
        let mut state = entry.state();
        if state.active_waiter.is_some() {
            return Err(ExecutionError::ExecutionBusy);
        }
        state.active_waiter = Some(waiter);
        Ok(waiter)
    }

    pub fn external_wait_preempted(&self, execution_id: &str, waiter: u64) -> bool {
        self.find(execution_id)
            .map_or(true, |entry| entry.state().preempted_waiter == Some(waiter))
    }

    pub fn release_external_wait(&self, execution_id: &str, waiter: u64) {
        if let Ok(entry) = self.find(execution_id) {
            let mut state = entry.state();
            if state.active_waiter == Some(waiter) {
                state.active_waiter = None;
            }
            if state.preempted_waiter == Some(waiter) {
                state.preempted_waiter = None;
            }
        }
    }

    pub fn preempt_wait(&self, execution_id: &str) {
        if let Ok(entry) = self.find(execution_id) {
            let mut state = entry.state();
            if let Some(waiter) = state.active_waiter {
                state.preempted_waiter = Some(waiter);
            }
        }
    }

    fn insert_tty(
        &self,
        capacity: Option<TtyCapacity>,
        command: &TtyCommand<'_>,
        update: &TtyUpdate<'_>,
    ) -> Result<Arc<Entry>, ExecutionError> {
        if capacity
            .as_ref()
            .is_some_and(|capacity| !Arc::ptr_eq(&capacity.shared, &self.shared))
        {
            return Err(ExecutionError::InvalidCapacityReservation);
        }
        let mut registry = self.shared.registry();
        if registry.stopping {
            return Err(ExecutionError::RuntimeStopping);
        }
        let existing = registry
            .entries
            .iter()
            .any(|entry| entry.id == update.execution_id);
        if existing {
            if let Some(capacity) = capacity {
                capacity.release_in(&mut registry);
            }
            drop(registry);
            let entry = self.live_tty(update.execution_id)?;
            apply(&mut entry.state(), update);
            return Ok(entry);
        }
        if capacity.is_none() && registry.live_count() >= MAX_LIVE_ENTRIES {
            return Err(ExecutionError::ExecutionCapacityExceeded);
        }
        evict_oldest_tombstone(&mut registry);
        let entry = Arc::new(Entry {
            id: update.execution_id.to_owned(),
            command: command.command.to_owned(),
            cwd: command.cwd.to_path_buf(),
            backend: Backend::Tty,
            state: Mutex::new(initial_state(command, update)),
            stop: watch::Sender::new(None),
            finished: watch::Sender::new(false),
        });
        registry.entries.push(Arc::clone(&entry));
        if let Some(capacity) = capacity {
            capacity.release_in(&mut registry);
        }
        Ok(entry)
    }

    fn live_tty(&self, execution_id: &str) -> Result<Arc<Entry>, ExecutionError> {
        let entry = self.find(execution_id)?;
        if entry.backend != Backend::Tty || entry.state().tombstone.is_some() {
            return Err(ExecutionError::InvalidBackend);
        }
        Ok(entry)
    }
}

fn initial_state(command: &TtyCommand<'_>, update: &TtyUpdate<'_>) -> EntryState {
    let mut state = EntryState::new(
        Phase::from_snapshot_state(update.state),
        command.max_output_bytes,
    );
    if update.published_running {
        state.claim = Claim::Published;
    }
    state.stdout_bytes = update.raw_bytes;
    state.append_bounded(update.output);
    state.output_incomplete = update.output_incomplete;
    state.error_name = update.error_name;
    state
}

fn apply(state: &mut EntryState, update: &TtyUpdate<'_>) {
    if !state.phase.is_terminal() {
        state.phase = Phase::from_snapshot_state(update.state);
    }
    if update.published_running {
        state.claim = Claim::Published;
    }
    state.stdout_bytes = state.stdout_bytes.saturating_add(update.raw_bytes);
    state.append_bounded(update.output);
    state.output_truncated |= update.output_incomplete;
    state.output_incomplete |= update.output_incomplete;
    if let Some(error_name) = update.error_name {
        state.error_name = Some(error_name);
    }
}
