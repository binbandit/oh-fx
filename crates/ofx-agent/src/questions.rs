use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_contract::{BoxFuture, QuestionAsker, QuestionBatchEntry, QuestionRequest, RequestId};
use tokio::sync::{Notify, oneshot};

type Answers = Option<Vec<String>>;

#[derive(Debug, Clone)]
pub struct Questions {
    shared: Arc<Shared>,
}

#[derive(Debug)]
pub struct QuestionRequests {
    shared: Arc<Shared>,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    announced: Notify,
}

#[derive(Debug, Default)]
struct State {
    issued: u64,
    closed: bool,
    unannounced: VecDeque<QuestionRequest>,
    pending: HashMap<RequestId, oneshot::Sender<Answers>>,
}

struct Pending {
    id: RequestId,
    shared: Arc<Shared>,
}

impl Questions {
    pub fn new() -> (Self, QuestionRequests) {
        let shared: Arc<Shared> = Arc::default();
        (
            Self {
                shared: Arc::clone(&shared),
            },
            QuestionRequests { shared },
        )
    }

    pub fn resolve(&self, id: RequestId, answers: Answers) -> bool {
        let sender = lock(&self.shared).pending.remove(&id);
        sender.is_some_and(|sender| sender.send(answers).is_ok())
    }
}

impl QuestionAsker for Questions {
    fn ask(&self, entries: Vec<QuestionBatchEntry>) -> BoxFuture<'static, Answers> {
        let mut state = lock(&self.shared);
        if state.closed {
            return Box::pin(async { None });
        }
        let (sender, answers) = oneshot::channel();
        state.issued += 1;
        let id = RequestId::new(state.issued);
        state.pending.insert(id, sender);
        state.unannounced.push_back(QuestionRequest { id, entries });
        drop(state);
        self.shared.announced.notify_one();
        let pending = Pending {
            id,
            shared: Arc::clone(&self.shared),
        };
        Box::pin(async move {
            let _pending = pending;
            answers.await.ok().flatten()
        })
    }
}

impl QuestionRequests {
    pub async fn next(&mut self) -> QuestionRequest {
        loop {
            let announced = self.shared.announced.notified();
            if let Some(request) = lock(&self.shared).unannounced.pop_front() {
                return request;
            }
            announced.await;
        }
    }
}

impl Drop for QuestionRequests {
    fn drop(&mut self) {
        let mut state = lock(&self.shared);
        state.closed = true;
        state.unannounced.clear();
        state.pending.clear();
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        let mut state = lock(&self.shared);
        state.pending.remove(&self.id);
        state.unannounced.retain(|request| request.id != self.id);
    }
}

fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(PoisonError::into_inner)
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

    fn is_empty(questions: &Questions) -> bool {
        let state = lock(&questions.shared);
        state.pending.is_empty() && state.unannounced.is_empty()
    }

    #[tokio::test]
    async fn each_question_is_announced_with_its_own_id_and_takes_one_answer() {
        let (questions, mut requests) = Questions::new();
        let first = questions.ask(entries("First?"));
        let second = questions.ask(entries("Second?"));
        let announced = requests.next().await;
        assert_eq!(announced.entries, entries("First?"));
        let next = requests.next().await;
        assert_ne!(announced.id, next.id);
        assert!(questions.resolve(announced.id, Some(vec!["Yes".to_owned()])));
        assert!(!questions.resolve(announced.id, Some(vec!["No".to_owned()])));
        assert!(questions.resolve(next.id, None));
        assert_eq!(first.await, Some(vec!["Yes".to_owned()]));
        assert_eq!(second.await, None);
        assert!(is_empty(&questions));
    }

    #[tokio::test]
    async fn a_question_asked_while_the_host_waits_wakes_it() {
        let (questions, mut requests) = Questions::new();
        let waiting = tokio::spawn(async move {
            let announced = requests.next().await;
            (requests, announced)
        });
        tokio::task::yield_now().await;
        let asked = questions.ask(entries("Late?"));
        let (_requests, announced) = waiting.await.unwrap();
        assert_eq!(announced.entries, entries("Late?"));
        assert!(questions.resolve(announced.id, Some(vec!["No".to_owned()])));
        assert_eq!(asked.await, Some(vec!["No".to_owned()]));
    }

    #[tokio::test]
    async fn a_question_abandoned_before_it_was_announced_is_never_announced_or_kept() {
        let (questions, mut requests) = Questions::new();
        drop(questions.ask(entries("Gone?")));
        assert!(is_empty(&questions));
        let kept = questions.ask(entries("Kept?"));
        let announced = requests.next().await;
        assert_eq!(announced.entries, entries("Kept?"));
        assert!(questions.resolve(announced.id, None));
        assert_eq!(kept.await, None);
    }

    #[tokio::test]
    async fn an_abandoned_question_cannot_be_answered() {
        let (questions, mut requests) = Questions::new();
        let abandoned = questions.ask(entries("Gone?"));
        let id = requests.next().await.id;
        drop(abandoned);
        assert!(!questions.resolve(id, Some(vec!["Yes".to_owned()])));
        assert!(is_empty(&questions));
    }

    #[tokio::test]
    async fn a_closed_host_answers_every_question_with_nothing() {
        let (questions, mut requests) = Questions::new();
        let announced = questions.ask(entries("Announced?"));
        requests.next().await;
        let queued = questions.ask(entries("Queued?"));
        drop(requests);
        assert_eq!(announced.await, None);
        assert_eq!(queued.await, None);
        assert_eq!(questions.ask(entries("Closed?")).await, None);
        assert!(is_empty(&questions));
    }
}
