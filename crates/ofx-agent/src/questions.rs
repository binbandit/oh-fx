use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_contract::{BoxFuture, QuestionAsker, QuestionBatchEntry, QuestionRequest, RequestId};
use tokio::sync::{mpsc, oneshot};

type Answers = Option<Vec<String>>;

#[derive(Debug, Clone)]
pub struct Questions {
    state: Arc<Mutex<State>>,
    requests: mpsc::UnboundedSender<QuestionRequest>,
}

#[derive(Debug)]
pub struct QuestionRequests(mpsc::UnboundedReceiver<QuestionRequest>);

#[derive(Debug, Default)]
struct State {
    issued: u64,
    pending: HashMap<RequestId, oneshot::Sender<Answers>>,
}

struct Pending {
    id: RequestId,
    state: Arc<Mutex<State>>,
}

impl Questions {
    pub fn new() -> (Self, QuestionRequests) {
        let (requests, receiver) = mpsc::unbounded_channel();
        (
            Self {
                state: Arc::default(),
                requests,
            },
            QuestionRequests(receiver),
        )
    }

    pub fn resolve(&self, id: RequestId, answers: Answers) -> bool {
        let sender = lock(&self.state).pending.remove(&id);
        sender.is_some_and(|sender| sender.send(answers).is_ok())
    }
}

impl QuestionAsker for Questions {
    fn ask(&self, entries: Vec<QuestionBatchEntry>) -> BoxFuture<'static, Answers> {
        let (sender, answers) = oneshot::channel();
        let mut state = lock(&self.state);
        state.issued += 1;
        let id = RequestId::new(state.issued);
        state.pending.insert(id, sender);
        drop(state);
        let pending = Pending {
            id,
            state: Arc::clone(&self.state),
        };
        let delivered = self.requests.send(QuestionRequest { id, entries }).is_ok();
        Box::pin(async move {
            let _pending = pending;
            if !delivered {
                return None;
            }
            answers.await.ok().flatten()
        })
    }
}

impl QuestionRequests {
    pub async fn next(&mut self) -> Option<QuestionRequest> {
        self.0.recv().await
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        lock(&self.state).pending.remove(&self.id);
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use ofx_contract::QuestionOption;

    use super::*;

    fn entries(question: &str) -> Vec<QuestionBatchEntry> {
        vec![QuestionBatchEntry {
            question: question.to_owned(),
            options: vec![
                QuestionOption {
                    label: "Yes".to_owned(),
                    description: None,
                },
                QuestionOption {
                    label: "No".to_owned(),
                    description: None,
                },
            ],
        }]
    }

    #[tokio::test]
    async fn each_question_is_announced_with_its_own_id_and_takes_one_answer() {
        let (questions, mut requests) = Questions::new();
        let first = questions.ask(entries("First?"));
        let second = questions.ask(entries("Second?"));
        let announced = requests.next().await.unwrap();
        assert_eq!(announced.entries, entries("First?"));
        let next = requests.next().await.unwrap();
        assert_ne!(announced.id, next.id);
        assert!(questions.resolve(announced.id, Some(vec!["Yes".to_owned()])));
        assert!(!questions.resolve(announced.id, Some(vec!["No".to_owned()])));
        assert!(questions.resolve(next.id, None));
        assert_eq!(first.await, Some(vec!["Yes".to_owned()]));
        assert_eq!(second.await, None);
    }

    #[tokio::test]
    async fn an_abandoned_question_cannot_be_answered_and_a_closed_host_answers_nothing() {
        let (questions, mut requests) = Questions::new();
        let abandoned = questions.ask(entries("Gone?"));
        let id = requests.next().await.unwrap().id;
        drop(abandoned);
        assert!(!questions.resolve(id, Some(vec!["Yes".to_owned()])));
        drop(requests);
        assert_eq!(questions.ask(entries("Closed?")).await, None);
        assert!(lock(&questions.state).pending.is_empty());
    }
}
