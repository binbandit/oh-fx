use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_contract::{
    BoxFuture, QuestionAsker, QuestionBatchEntry, QuestionRequest, QuestionSource, RequestId,
};
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
    withdrawn: VecDeque<RequestId>,
    pending: HashMap<RequestId, oneshot::Sender<Answers>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionEvent {
    Asked(QuestionRequest),
    Withdrawn(RequestId),
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

    pub fn elicitation(&self) -> Arc<dyn QuestionAsker> {
        Arc::new(Elicitation(self.clone()))
    }

    fn ask_from(
        &self,
        source: QuestionSource,
        entries: Vec<QuestionBatchEntry>,
    ) -> BoxFuture<'static, Answers> {
        let mut state = lock(&self.shared);
        if state.closed {
            return Box::pin(async { None });
        }
        let (sender, answers) = oneshot::channel();
        state.issued += 1;
        let id = RequestId::new(state.issued);
        state.pending.insert(id, sender);
        state.unannounced.push_back(QuestionRequest {
            id,
            entries,
            source,
        });
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

impl QuestionAsker for Questions {
    fn ask(&self, entries: Vec<QuestionBatchEntry>) -> BoxFuture<'static, Answers> {
        self.ask_from(QuestionSource::AgentQuestion, entries)
    }
}

struct Elicitation(Questions);

impl QuestionAsker for Elicitation {
    fn ask(&self, entries: Vec<QuestionBatchEntry>) -> BoxFuture<'static, Answers> {
        self.0.ask_from(QuestionSource::McpElicitation, entries)
    }
}

impl QuestionRequests {
    pub async fn next(&mut self) -> QuestionEvent {
        loop {
            let announced = self.shared.announced.notified();
            {
                let mut state = lock(&self.shared);
                if let Some(id) = state.withdrawn.pop_front() {
                    return QuestionEvent::Withdrawn(id);
                }
                if let Some(request) = state.unannounced.pop_front() {
                    return QuestionEvent::Asked(request);
                }
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
        state.withdrawn.clear();
        state.pending.clear();
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        let mut state = lock(&self.shared);
        let waiting = state.pending.remove(&self.id).is_some();
        let unannounced = state.unannounced.len();
        state.unannounced.retain(|request| request.id != self.id);
        if waiting && unannounced == state.unannounced.len() && !state.closed {
            state.withdrawn.push_back(self.id);
            drop(state);
            self.shared.announced.notify_one();
        }
    }
}

fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use ofx_contract::{QuestionOption, QuestionSubmission};

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
            submission: QuestionSubmission::Label,
        }]
    }

    async fn asked(requests: &mut QuestionRequests) -> QuestionRequest {
        match requests.next().await {
            QuestionEvent::Asked(request) => request,
            QuestionEvent::Withdrawn(id) => panic!("{id:?} was withdrawn"),
        }
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
        let announced = asked(&mut requests).await;
        assert_eq!(announced.entries, entries("First?"));
        let next = asked(&mut requests).await;
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
            let announced = asked(&mut requests).await;
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
        let announced = asked(&mut requests).await;
        assert_eq!(announced.entries, entries("Kept?"));
        assert!(questions.resolve(announced.id, None));
        assert_eq!(kept.await, None);
    }

    #[tokio::test]
    async fn an_abandoned_question_cannot_be_answered_and_is_withdrawn() {
        let (questions, mut requests) = Questions::new();
        let abandoned = questions.ask(entries("Gone?"));
        let id = asked(&mut requests).await.id;
        drop(abandoned);
        assert!(!questions.resolve(id, Some(vec!["Yes".to_owned()])));
        assert!(is_empty(&questions));
        assert_eq!(requests.next().await, QuestionEvent::Withdrawn(id));
        let answered = questions.ask(entries("Answered?"));
        let id = asked(&mut requests).await.id;
        assert!(questions.resolve(id, None));
        assert_eq!(answered.await, None);
        let next = questions.ask(entries("Next?"));
        assert_eq!(asked(&mut requests).await.entries, entries("Next?"));
        drop(next);
    }

    #[tokio::test]
    async fn a_closed_host_answers_every_question_with_nothing() {
        let (questions, mut requests) = Questions::new();
        let announced = questions.ask(entries("Announced?"));
        asked(&mut requests).await;
        let queued = questions.ask(entries("Queued?"));
        drop(requests);
        assert_eq!(announced.await, None);
        assert_eq!(queued.await, None);
        assert_eq!(questions.ask(entries("Closed?")).await, None);
        assert!(is_empty(&questions));
    }

    #[tokio::test]
    async fn elicitation_questions_are_announced_with_their_source() {
        let (questions, mut requests) = Questions::new();
        let agent = questions.ask(entries("Agent?"));
        let elicited = questions.elicitation().ask(entries("Server?"));
        assert_eq!(
            asked(&mut requests).await.source,
            QuestionSource::AgentQuestion
        );
        let announced = asked(&mut requests).await;
        assert_eq!(announced.source, QuestionSource::McpElicitation);
        assert!(questions.resolve(announced.id, Some(vec!["Yes".to_owned()])));
        assert_eq!(elicited.await, Some(vec!["Yes".to_owned()]));
        drop(agent);
    }
}
