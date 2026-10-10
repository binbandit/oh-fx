use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

const RING_CAPACITY: usize = 32;
const TURN_ROLLUP_CAPACITY: usize = 16;
const MAX_MODEL_BYTES: usize = 64;
const MAX_ERROR_BYTES: usize = 48;
const MAX_STOP_REASON_BYTES: usize = 48;
const FIRST_ERROR_STATUS: u16 = 400;

pub static NETWORK_CALLS: NetworkRing = NetworkRing::new();

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NetworkCallKind {
    #[default]
    Gateway,
    WebFetchTarget,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkCall {
    pub kind: NetworkCallKind,
    pub started_at_ms: i64,
    pub duration_ms: u32,
    pub status: u16,
    pub response_bytes: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub turn_id: u64,
    pub step_id: u64,
    pub subagent_id: u64,
    pub model: String,
    pub error: String,
    pub stop_reason: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetworkLifetime {
    pub total_calls: u64,
    pub ok_calls: u64,
    pub error_calls: u64,
    pub total_duration_ms: u64,
    pub evicted_turns: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetworkTurnRollup {
    pub turn_id: u64,
    pub calls: u32,
    pub error_calls: u32,
    pub subagent_calls: u32,
    pub total_duration_ms: u64,
    pub first_started_at_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkTrace {
    pub calls: Vec<NetworkCall>,
    pub lifetime: NetworkLifetime,
    pub turns: Vec<NetworkTurnRollup>,
}

pub struct NetworkRing {
    state: Mutex<State>,
}

struct State {
    calls: VecDeque<NetworkCall>,
    lifetime: NetworkLifetime,
    turns: Vec<NetworkTurnRollup>,
}

impl NetworkCallKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::WebFetchTarget => "web_fetch_target",
        }
    }
}

impl NetworkCall {
    pub fn is_error(&self) -> bool {
        !self.error.is_empty() || self.status >= FIRST_ERROR_STATUS
    }

    fn bounded(mut self) -> Self {
        cut(&mut self.model, MAX_MODEL_BYTES);
        cut(&mut self.error, MAX_ERROR_BYTES);
        cut(&mut self.stop_reason, MAX_STOP_REASON_BYTES);
        self
    }
}

pub fn network_trace() -> NetworkTrace {
    NETWORK_CALLS.snapshot()
}

pub fn reset_network_trace() {
    NETWORK_CALLS.reset();
}

impl NetworkRing {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(State {
                calls: VecDeque::new(),
                lifetime: NetworkLifetime {
                    total_calls: 0,
                    ok_calls: 0,
                    error_calls: 0,
                    total_duration_ms: 0,
                    evicted_turns: 0,
                },
                turns: Vec::new(),
            }),
        }
    }

    pub fn record(&self, call: NetworkCall) {
        let call = call.bounded();
        let mut state = self.lock();
        state.count(&call);
        if state.calls.len() == RING_CAPACITY {
            state.calls.pop_front();
        }
        state.calls.push_back(call);
    }

    pub fn snapshot(&self) -> NetworkTrace {
        let state = self.lock();
        let mut turns = state.turns.clone();
        turns.sort_by_key(|rollup| rollup.turn_id);
        NetworkTrace {
            calls: state.calls.iter().cloned().collect(),
            lifetime: state.lifetime,
            turns,
        }
    }

    pub fn reset(&self) {
        let mut state = self.lock();
        state.calls.clear();
        state.lifetime = NetworkLifetime::default();
        state.turns.clear();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for NetworkRing {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    fn count(&mut self, call: &NetworkCall) {
        let failed = call.is_error();
        let lifetime = &mut self.lifetime;
        lifetime.total_calls = lifetime.total_calls.saturating_add(1);
        if failed {
            lifetime.error_calls = lifetime.error_calls.saturating_add(1);
        } else {
            lifetime.ok_calls = lifetime.ok_calls.saturating_add(1);
        }
        lifetime.total_duration_ms = lifetime
            .total_duration_ms
            .saturating_add(u64::from(call.duration_ms));
        if call.turn_id == 0 {
            return;
        }
        let rollup = self.rollup_for(call.turn_id);
        rollup.calls = rollup.calls.saturating_add(1);
        if failed {
            rollup.error_calls = rollup.error_calls.saturating_add(1);
        }
        if call.subagent_id != 0 {
            rollup.subagent_calls = rollup.subagent_calls.saturating_add(1);
        }
        rollup.total_duration_ms = rollup
            .total_duration_ms
            .saturating_add(u64::from(call.duration_ms));
        if call.started_at_ms > 0
            && (rollup.first_started_at_ms == 0 || call.started_at_ms < rollup.first_started_at_ms)
        {
            rollup.first_started_at_ms = call.started_at_ms;
        }
    }

    fn rollup_for(&mut self, turn_id: u64) -> &mut NetworkTurnRollup {
        let fresh = NetworkTurnRollup {
            turn_id,
            ..NetworkTurnRollup::default()
        };
        let index = match self
            .turns
            .iter()
            .position(|rollup| rollup.turn_id == turn_id)
        {
            Some(index) => index,
            None if self.turns.len() < TURN_ROLLUP_CAPACITY => {
                self.turns.push(fresh);
                self.turns.len() - 1
            }
            None => {
                let coldest = self
                    .turns
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, rollup)| rollup.turn_id)
                    .map_or(0, |(index, _)| index);
                self.lifetime.evicted_turns = self.lifetime.evicted_turns.saturating_add(1);
                self.turns[coldest] = fresh;
                coldest
            }
        };
        &mut self.turns[index]
    }
}

fn cut(text: &mut String, limit: usize) {
    let end = text.floor_char_boundary(limit);
    text.truncate(end);
}

#[cfg(test)]
mod tests;
