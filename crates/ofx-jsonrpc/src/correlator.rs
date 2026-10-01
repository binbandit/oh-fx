use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::oneshot;
use tokio::time::{Instant, timeout_at};

use crate::jsonrpc::RequestId;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegisterError<E> {
    #[error("duplicate request id")]
    Duplicate,
    #[error("correlator closed")]
    Closed(E),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WaitError<E> {
    #[error("request failed")]
    Failed(E),
    #[error("request timed out")]
    TimedOut,
    #[error("request abandoned")]
    Abandoned,
}

type Sender<T, E> = oneshot::Sender<Result<T, E>>;

struct State<T, E> {
    pending: HashMap<RequestId, Sender<T, E>>,
    closed: Option<E>,
}

pub struct Correlator<T, E> {
    state: Arc<Mutex<State<T, E>>>,
}

impl<T, E> Clone for Correlator<T, E> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl<T, E> Default for Correlator<T, E> {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                pending: HashMap::new(),
                closed: None,
            })),
        }
    }
}

impl<T, E: Clone> Correlator<T, E> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, id: RequestId) -> Result<PendingResponse<T, E>, RegisterError<E>> {
        let mut state = lock(&self.state);
        if let Some(error) = &state.closed {
            return Err(RegisterError::Closed(error.clone()));
        }
        if state.pending.contains_key(&id) {
            return Err(RegisterError::Duplicate);
        }
        let (sender, receiver) = oneshot::channel();
        state.pending.insert(id.clone(), sender);
        Ok(PendingResponse {
            id,
            receiver,
            state: Arc::clone(&self.state),
        })
    }

    pub fn resolve(&self, id: &RequestId, outcome: Result<T, E>) -> bool {
        let sender = lock(&self.state).pending.remove(id);
        sender.is_some_and(|sender| sender.send(outcome).is_ok())
    }

    pub fn close(&self, error: E) {
        let senders: Vec<_> = {
            let mut state = lock(&self.state);
            if state.closed.is_none() {
                state.closed = Some(error.clone());
            }
            state.pending.drain().map(|(_, sender)| sender).collect()
        };
        for sender in senders {
            let _ = sender.send(Err(error.clone()));
        }
    }
}

pub struct PendingResponse<T, E> {
    id: RequestId,
    receiver: oneshot::Receiver<Result<T, E>>,
    state: Arc<Mutex<State<T, E>>>,
}

impl<T, E> PendingResponse<T, E> {
    pub async fn wait_until(mut self, deadline: Instant) -> Result<T, WaitError<E>> {
        if let Ok(received) = timeout_at(deadline, &mut self.receiver).await {
            return match received {
                Ok(outcome) => outcome.map_err(WaitError::Failed),
                Err(_) => Err(WaitError::Abandoned),
            };
        }
        lock(&self.state).pending.remove(&self.id);
        match self.receiver.try_recv() {
            Ok(outcome) => outcome.map_err(WaitError::Failed),
            Err(_) => Err(WaitError::TimedOut),
        }
    }
}

impl<T, E> Drop for PendingResponse<T, E> {
    fn drop(&mut self) {
        lock(&self.state).pending.remove(&self.id);
    }
}

fn lock<S>(mutex: &Mutex<S>) -> MutexGuard<'_, S> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    type TestCorrelator = Correlator<String, &'static str>;

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn pending_count(correlator: &TestCorrelator) -> usize {
        lock(&correlator.state).pending.len()
    }

    #[tokio::test]
    async fn delivers_responses_to_the_matching_request() {
        let correlator = TestCorrelator::new();
        let first = correlator.register(RequestId::Integer(1)).unwrap();
        let second = correlator.register(RequestId::Integer(2)).unwrap();
        assert!(correlator.resolve(&RequestId::Integer(2), Ok("two".to_owned())));
        assert!(correlator.resolve(&RequestId::Integer(1), Ok("one".to_owned())));
        assert_eq!(first.wait_until(far()).await, Ok("one".to_owned()));
        assert_eq!(second.wait_until(far()).await, Ok("two".to_owned()));
        assert_eq!(pending_count(&correlator), 0);
    }

    #[tokio::test]
    async fn rejects_duplicate_ids_and_ignores_unknown_responses() {
        let correlator = TestCorrelator::new();
        let _pending = correlator.register(RequestId::Integer(1)).unwrap();
        assert!(matches!(
            correlator.register(RequestId::Integer(1)),
            Err(RegisterError::Duplicate)
        ));
        assert!(!correlator.resolve(&RequestId::String("1".to_owned()), Ok(String::new())));
    }

    #[tokio::test]
    async fn dropping_a_pending_response_cancels_its_registration() {
        let correlator = TestCorrelator::new();
        let pending = correlator.register(RequestId::Integer(5)).unwrap();
        assert_eq!(pending_count(&correlator), 1);
        drop(pending);
        assert_eq!(pending_count(&correlator), 0);
        assert!(!correlator.resolve(&RequestId::Integer(5), Ok("late".to_owned())));
    }

    #[tokio::test]
    async fn timeouts_remove_the_registration() {
        let correlator = TestCorrelator::new();
        let pending = correlator.register(RequestId::Integer(3)).unwrap();
        let deadline = Instant::now() + Duration::from_millis(10);
        assert_eq!(pending.wait_until(deadline).await, Err(WaitError::TimedOut));
        assert_eq!(pending_count(&correlator), 0);
    }

    #[tokio::test]
    async fn closing_fails_waiters_and_refuses_new_requests() {
        let correlator = TestCorrelator::new();
        let pending = correlator.register(RequestId::Integer(1)).unwrap();
        correlator.close("McpConnectionClosed");
        assert_eq!(
            pending.wait_until(far()).await,
            Err(WaitError::Failed("McpConnectionClosed"))
        );
        assert!(matches!(
            correlator.register(RequestId::Integer(2)),
            Err(RegisterError::Closed("McpConnectionClosed"))
        ));
    }
}
