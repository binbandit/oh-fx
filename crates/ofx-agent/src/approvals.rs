use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_contract::{ApprovalAnswer, ApprovalDecision, RequestId};
use tokio::sync::oneshot;

type WithdrawnListener = Arc<dyn Fn(RequestId) + Send + Sync>;

#[derive(Clone, Default)]
pub struct Approvals {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    issued: u64,
    pending: HashMap<RequestId, oneshot::Sender<ApprovalAnswer>>,
    withdrawn: Option<WithdrawnListener>,
}

pub(crate) struct PendingApproval {
    id: RequestId,
    answer: oneshot::Receiver<ApprovalAnswer>,
    approvals: Approvals,
}

impl Approvals {
    pub fn resolve(&self, id: RequestId, answer: impl Into<ApprovalAnswer>) -> bool {
        let sender = self.lock().pending.remove(&id);
        sender.is_some_and(|sender| sender.send(answer.into()).is_ok())
    }

    pub fn on_withdrawn(&self, listener: impl Fn(RequestId) + Send + Sync + 'static) {
        self.lock().withdrawn = Some(Arc::new(listener));
    }

    pub(crate) fn open(&self) -> PendingApproval {
        let (sender, answer) = oneshot::channel();
        let mut state = self.lock();
        state.issued += 1;
        let id = RequestId::new(state.issued);
        state.pending.insert(id, sender);
        drop(state);
        PendingApproval {
            id,
            answer,
            approvals: self.clone(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PendingApproval {
    pub(crate) fn id(&self) -> RequestId {
        self.id
    }

    pub(crate) async fn answer(&mut self) -> ApprovalAnswer {
        (&mut self.answer)
            .await
            .unwrap_or_else(|_| ApprovalDecision::Deny.into())
    }

    pub(crate) fn withdraw(&mut self) -> Option<ApprovalAnswer> {
        self.answer.close();
        self.answer.try_recv().ok()
    }
}

impl Drop for PendingApproval {
    fn drop(&mut self) {
        let mut state = self.approvals.lock();
        let unanswered = state.pending.remove(&self.id).is_some();
        let listener = state.withdrawn.clone();
        drop(state);
        if let (true, Some(listener)) = (unanswered, listener) {
            listener(self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn each_request_gets_its_own_id_and_takes_one_answer_with_its_feedback() {
        let approvals = Approvals::default();
        let mut first = approvals.open();
        let second = approvals.open();
        assert_ne!(first.id(), second.id());
        let answer = ApprovalAnswer {
            decision: ApprovalDecision::Always,
            feedback: Some("read the tests next".to_owned()),
        };
        assert!(approvals.resolve(first.id(), answer.clone()));
        assert!(!approvals.resolve(first.id(), ApprovalDecision::Once));
        assert_eq!(first.answer().await, answer);
    }

    #[test]
    fn a_withdrawn_request_keeps_an_answer_already_given_and_refuses_later_ones() {
        let approvals = Approvals::default();
        let mut unanswered = approvals.open();
        assert_eq!(unanswered.withdraw(), None);
        assert!(!approvals.resolve(unanswered.id(), ApprovalDecision::Always));
        let mut answered = approvals.open();
        assert!(approvals.resolve(answered.id(), ApprovalDecision::Always));
        assert_eq!(answered.withdraw(), Some(ApprovalDecision::Always.into()));
        assert!(!approvals.resolve(answered.id(), ApprovalDecision::Once));
    }

    #[test]
    fn requests_left_unanswered_are_reported_withdrawn_and_answered_ones_are_not() {
        let approvals = Approvals::default();
        let withdrawn = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&withdrawn);
        approvals.on_withdrawn(move |id| seen.lock().unwrap().push(id));
        let answered = approvals.open();
        let mut cancelled = approvals.open();
        let abandoned = approvals.open();
        assert!(approvals.resolve(answered.id(), ApprovalDecision::Once));
        drop(answered);
        assert_eq!(cancelled.withdraw(), None);
        let (cancelled_id, abandoned_id) = (cancelled.id(), abandoned.id());
        drop(cancelled);
        drop(abandoned);
        assert_eq!(*withdrawn.lock().unwrap(), [cancelled_id, abandoned_id]);
    }

    #[tokio::test]
    async fn abandoned_requests_cannot_be_resolved_and_unanswered_ones_deny() {
        let approvals = Approvals::default();
        let abandoned = approvals.open();
        let id = abandoned.id();
        drop(abandoned);
        assert!(!approvals.resolve(id, ApprovalDecision::Once));
        let mut orphaned = approvals.open();
        approvals.lock().pending.clear();
        assert_eq!(orphaned.answer().await, ApprovalDecision::Deny.into());
    }
}
