use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::error::McpError;

const MAX_WAITERS: usize = 32;
const MAX_EARLY_COMPLETIONS: usize = 64;
const EARLY_COMPLETION_TTL: Duration = Duration::from_mins(10);

#[derive(Debug, Default)]
pub(crate) struct UrlCompletions {
    state: Mutex<State>,
    closed: CancellationToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Completion {
    Pending,
    Completed,
    Cancelled,
}

#[derive(Debug, Default)]
struct State {
    waiters: Vec<Arc<WaiterState>>,
    early: VecDeque<(String, Instant)>,
}

#[derive(Debug)]
struct WaiterState {
    ids: Vec<String>,
    completed: Mutex<Vec<bool>>,
    status: watch::Sender<Completion>,
}

pub(crate) struct Waiter<'a> {
    completions: &'a UrlCompletions,
    state: Arc<WaiterState>,
}

impl UrlCompletions {
    pub(crate) fn wait_for(&self, ids: &[String]) -> Result<Waiter<'_>, McpError> {
        let mut state = lock(&self.state);
        if self.closed.is_cancelled() {
            return Err(McpError::Cancelled);
        }
        if state.waiters.len() >= MAX_WAITERS {
            return Err(McpError::McpInputRequiredLimitExceeded);
        }
        if state
            .waiters
            .iter()
            .any(|waiter| waiter.ids.iter().any(|id| ids.contains(id)))
        {
            return Err(McpError::McpDuplicateElicitationId);
        }
        let waiter = Arc::new(WaiterState {
            ids: ids.to_vec(),
            completed: Mutex::new(vec![false; ids.len()]),
            status: watch::Sender::new(Completion::Pending),
        });
        state.prune(Instant::now());
        state.early.retain(|(id, _)| !waiter.complete(id));
        state.waiters.push(Arc::clone(&waiter));
        Ok(Waiter {
            completions: self,
            state: waiter,
        })
    }

    pub(crate) fn record(&self, id: &str) {
        let mut state = lock(&self.state);
        if self.closed.is_cancelled() {
            return;
        }
        if state
            .waiters
            .iter()
            .any(|waiter| waiter.ids.iter().any(|candidate| candidate == id))
        {
            for waiter in &state.waiters {
                waiter.complete(id);
            }
            return;
        }
        let now = Instant::now();
        state.prune(now);
        if state.early.iter().any(|(early, _)| early == id) {
            return;
        }
        if state.early.len() >= MAX_EARLY_COMPLETIONS {
            state.early.pop_front();
        }
        state.early.push_back((id.to_owned(), now));
    }

    pub(crate) fn close(&self) {
        let mut state = lock(&self.state);
        self.closed.cancel();
        state.early.clear();
        for waiter in &state.waiters {
            waiter.settle(Completion::Cancelled);
        }
    }

    pub(crate) fn closed(&self) -> &CancellationToken {
        &self.closed
    }
}

impl State {
    fn prune(&mut self, now: Instant) {
        self.early
            .retain(|(_, recorded)| now.duration_since(*recorded) <= EARLY_COMPLETION_TTL);
    }
}

impl WaiterState {
    fn complete(&self, id: &str) -> bool {
        let Some(index) = self.ids.iter().position(|candidate| candidate == id) else {
            return false;
        };
        let mut completed = lock(&self.completed);
        completed[index] = true;
        if completed.iter().all(|done| *done) {
            self.settle(Completion::Completed);
        }
        true
    }

    fn settle(&self, outcome: Completion) {
        self.status.send_if_modified(|status| {
            let pending = *status == Completion::Pending;
            if pending {
                *status = outcome;
            }
            pending
        });
    }
}

impl Waiter<'_> {
    pub(crate) fn status(&self) -> Completion {
        *self.state.status.borrow()
    }

    pub(crate) async fn settled(&self) {
        let mut status = self.state.status.subscribe();
        let _ = status
            .wait_for(|status| *status != Completion::Pending)
            .await;
    }
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        lock(&self.completions.state)
            .waiters
            .retain(|waiter| !Arc::ptr_eq(waiter, &self.state));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[tokio::test]
    async fn a_waiter_completes_only_when_every_bound_id_is_confirmed() {
        let completions = UrlCompletions::default();
        let waiter = completions.wait_for(&ids(&["one", "two"])).unwrap();
        completions.record("other");
        completions.record("one");
        assert_eq!(waiter.status(), Completion::Pending);
        completions.record("one");
        assert_eq!(waiter.status(), Completion::Pending);
        completions.record("two");
        assert_eq!(waiter.status(), Completion::Completed);
        waiter.settled().await;
    }

    #[test]
    fn a_completion_that_arrives_first_is_replayed_to_its_waiter() {
        let completions = UrlCompletions::default();
        completions.record("early");
        let waiter = completions.wait_for(&ids(&["early"])).unwrap();
        assert_eq!(waiter.status(), Completion::Completed);
        drop(waiter);
        let again = completions.wait_for(&ids(&["early"])).unwrap();
        assert_eq!(again.status(), Completion::Pending);
    }

    #[test]
    fn waiters_are_bounded_unique_and_released_when_dropped() {
        let completions = UrlCompletions::default();
        let first = completions.wait_for(&ids(&["a"])).unwrap();
        assert!(matches!(
            completions.wait_for(&ids(&["b", "a"])),
            Err(McpError::McpDuplicateElicitationId)
        ));
        drop(first);
        let held: Vec<_> = (0..MAX_WAITERS)
            .map(|index| completions.wait_for(&[index.to_string()]).unwrap())
            .collect();
        assert!(matches!(
            completions.wait_for(&ids(&["late"])),
            Err(McpError::McpInputRequiredLimitExceeded)
        ));
        drop(held);
        for index in 0..=MAX_EARLY_COMPLETIONS {
            completions.record(&format!("early-{index}"));
        }
        assert_eq!(
            completions.wait_for(&ids(&["early-0"])).unwrap().status(),
            Completion::Pending
        );
        assert_eq!(
            completions.wait_for(&ids(&["early-1"])).unwrap().status(),
            Completion::Completed
        );
    }

    #[tokio::test]
    async fn a_closed_connection_cancels_its_waiters_and_refuses_new_ones() {
        let completions = UrlCompletions::default();
        let completed = completions.wait_for(&ids(&["done"])).unwrap();
        completions.record("done");
        let waiting = completions.wait_for(&ids(&["open"])).unwrap();
        completions.record("early");
        completions.close();
        waiting.settled().await;
        assert_eq!(waiting.status(), Completion::Cancelled);
        assert_eq!(completed.status(), Completion::Completed);
        assert!(completions.closed().is_cancelled());
        completions.record("open");
        assert_eq!(waiting.status(), Completion::Cancelled);
        assert!(matches!(
            completions.wait_for(&ids(&["early"])),
            Err(McpError::Cancelled)
        ));
    }
}
