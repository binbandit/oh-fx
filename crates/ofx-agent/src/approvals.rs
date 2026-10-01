use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_contract::{ApprovalDecision, RequestId};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Default)]
pub struct Approvals {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    issued: u64,
    pending: HashMap<RequestId, oneshot::Sender<ApprovalDecision>>,
}

pub(crate) struct PendingApproval {
    id: RequestId,
    decision: oneshot::Receiver<ApprovalDecision>,
    approvals: Approvals,
}

impl Approvals {
    pub fn resolve(&self, id: RequestId, decision: ApprovalDecision) -> bool {
        let sender = self.lock().pending.remove(&id);
        sender.is_some_and(|sender| sender.send(decision).is_ok())
    }

    pub(crate) fn open(&self) -> PendingApproval {
        let (sender, decision) = oneshot::channel();
        let mut state = self.lock();
        state.issued += 1;
        let id = RequestId::new(state.issued);
        state.pending.insert(id, sender);
        drop(state);
        PendingApproval {
            id,
            decision,
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

    pub(crate) async fn decision(&mut self) -> ApprovalDecision {
        (&mut self.decision).await.unwrap_or(ApprovalDecision::Deny)
    }
}

impl Drop for PendingApproval {
    fn drop(&mut self) {
        self.approvals.lock().pending.remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn each_request_gets_its_own_id_and_takes_one_decision() {
        let approvals = Approvals::default();
        let mut first = approvals.open();
        let second = approvals.open();
        assert_ne!(first.id(), second.id());
        assert!(approvals.resolve(first.id(), ApprovalDecision::Always));
        assert!(!approvals.resolve(first.id(), ApprovalDecision::Once));
        assert_eq!(first.decision().await, ApprovalDecision::Always);
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
        assert_eq!(orphaned.decision().await, ApprovalDecision::Deny);
    }
}
