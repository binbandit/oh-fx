use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sequenced<T> {
    pub sequence: u64,
    pub event: T,
}

pub struct Ring<T> {
    capacity: usize,
    state: Mutex<State<T>>,
}

struct State<T> {
    events: VecDeque<Sequenced<T>>,
    total: u64,
}

impl<T> Ring<T> {
    pub const fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(State {
                events: VecDeque::new(),
                total: 0,
            }),
        }
    }

    pub fn record(&self, event: T) {
        let mut state = self.lock();
        state.total = state.total.saturating_add(1);
        if self.capacity == 0 {
            return;
        }
        if state.events.len() == self.capacity {
            state.events.pop_front();
        }
        let sequence = state.total;
        state.events.push_back(Sequenced { sequence, event });
    }

    pub fn reset(&self) {
        let mut state = self.lock();
        state.events.clear();
        state.total = 0;
    }

    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: Clone> Ring<T> {
    pub fn snapshot(&self) -> Vec<Sequenced<T>> {
        self.lock().events.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests;
