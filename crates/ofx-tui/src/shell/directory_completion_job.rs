use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use crate::composer::file_completion_state::{
    Anchor, CAPACITY, FileMatch, IndexRevision, LookupMode, State, Status,
};

pub type DirectoryLister =
    Arc<dyn Fn(&str, usize, &AtomicBool) -> Option<Vec<FileMatch>> + Send + Sync>;

const LISTER_THREAD: &str = "oh-fx-directory-completion";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Request {
    id: u64,
    episode: u64,
    anchor: Anchor,
    query: String,
}

impl Request {
    fn copy(id: u64, state: &State) -> Option<Self> {
        Some(Self {
            id,
            episode: state.episode,
            anchor: state.anchor.clone(),
            query: state.lookup_query.clone()?,
        })
    }

    fn matches(&self, state: &State) -> bool {
        state.mode == Some(LookupMode::Directory)
            && state.directory_request == Some(self.id)
            && state.episode == self.episode
            && state.anchor == self.anchor
            && state.lookup_query.as_deref() == Some(self.query.as_str())
    }
}

struct Task {
    request: Request,
    done: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    abandoned: bool,
    thread: JoinHandle<Option<Vec<FileMatch>>>,
}

impl Task {
    fn stop(&mut self) {
        if !self.abandoned {
            self.abandoned = true;
            self.cancel.store(true, Ordering::Release);
        }
    }
}

pub(super) struct DirectoryCompletionJob {
    lister: Option<DirectoryLister>,
    task: Option<Task>,
    pending: Option<Request>,
    next_request: u64,
}

impl DirectoryCompletionJob {
    pub(super) fn new(lister: Option<DirectoryLister>) -> Self {
        Self {
            lister,
            task: None,
            pending: None,
            next_request: 1,
        }
    }

    pub(super) fn is_busy(&self) -> bool {
        self.task.is_some()
    }

    pub(super) fn stop(&mut self) {
        if let Some(task) = &mut self.task {
            task.stop();
        }
        self.pending = None;
    }

    pub(super) fn reconcile(&mut self, state: &mut State, eligible: bool) {
        if let Some(task) = &mut self.task
            && (!eligible || !task.request.matches(state))
        {
            task.stop();
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|request| !eligible || !request.matches(state))
        {
            self.pending = None;
        }
        if !eligible && state.directory_request.is_some() {
            state.directory_request = None;
            state.require_lookup();
        }
    }

    pub(super) fn schedule(&mut self, state: &mut State) {
        let id = self.next_request;
        self.next_request = self.next_request.wrapping_add(1);
        state.directory_request = Some(id);
        self.reconcile(state, true);
        state.stage(IndexRevision::READY, Status::Loading, Vec::new());
        let Some(request) = Request::copy(id, state) else {
            fail(state, id);
            return;
        };
        if self.task.is_some() {
            self.pending = Some(request);
        } else if !self.start(request) {
            fail(state, id);
        }
    }

    pub(super) fn harvest(&mut self, state: &mut State, eligible: bool) -> bool {
        self.reconcile(state, eligible);
        if !self
            .task
            .as_ref()
            .is_some_and(|task| task.done.load(Ordering::Acquire))
        {
            return false;
        }
        let Some(task) = self.task.take() else {
            return false;
        };
        let outcome = task.thread.join().ok().flatten();
        if !task.abandoned && task.request.matches(state) {
            match outcome {
                Some(rows) => {
                    state.directory_request = None;
                    let status = if rows.is_empty() {
                        Status::Empty
                    } else {
                        Status::Ready
                    };
                    state.stage(IndexRevision::READY, status, rows);
                }
                None => fail(state, task.request.id),
            }
        }
        if let Some(request) = self.pending.take() {
            let id = request.id;
            if !self.start(request) {
                fail(state, id);
            }
        }
        true
    }

    fn start(&mut self, request: Request) -> bool {
        let Some(lister) = self.lister.clone() else {
            return false;
        };
        let done = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let (worker_done, worker_cancel) = (Arc::clone(&done), Arc::clone(&cancel));
        let query = request.query.clone();
        let spawned = thread::Builder::new()
            .name(LISTER_THREAD.to_owned())
            .spawn(move || {
                let rows = lister(&query, CAPACITY, &worker_cancel)
                    .filter(|_| !worker_cancel.load(Ordering::Acquire))
                    .map(|mut rows| {
                        rows.truncate(CAPACITY);
                        rows
                    });
                worker_done.store(true, Ordering::Release);
                rows
            });
        let Ok(thread) = spawned else {
            return false;
        };
        self.task = Some(Task {
            request,
            done,
            cancel,
            abandoned: false,
            thread,
        });
        true
    }
}

impl Drop for DirectoryCompletionJob {
    fn drop(&mut self) {
        self.stop();
    }
}

fn fail(state: &mut State, id: u64) {
    if state.directory_request != Some(id) {
        return;
    }
    state.directory_request = None;
    state.stage(IndexRevision::READY, Status::Unavailable, Vec::new());
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::composer::file_completion_state::MentionKind;
    use crate::composer::file_picker_path::query_at;

    fn bind(state: &mut State, text: &str) {
        state.reconcile(query_at(text, text.len()).as_ref(), false);
    }

    fn row(path: &str) -> FileMatch {
        FileMatch {
            path: path.to_owned(),
            kind: MentionKind::File,
            spans: Vec::new(),
        }
    }

    fn settle(job: &mut DirectoryCompletionJob, state: &mut State) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while job.is_busy() {
            job.harvest(state, true);
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn presented(state: &State) -> (Status, Vec<String>) {
        let view = state.view(0, 0);
        (
            view.status,
            view.items.iter().map(|item| item.path.clone()).collect(),
        )
    }

    #[test]
    fn listings_stage_rows_for_the_occurrence_they_were_asked_for() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&queries);
        let lister: DirectoryLister = Arc::new(move |query: &str, _: usize, _: &AtomicBool| {
            seen.lock().unwrap().push(query.to_owned());
            (query != "./missing/").then(|| vec![row(&format!("{query}a.txt"))])
        });
        let mut job = DirectoryCompletionJob::new(Some(lister));
        let mut state = State::default();
        bind(&mut state, "@./");
        job.schedule(&mut state);
        assert_eq!(presented(&state).0, Status::Loading);
        settle(&mut job, &mut state);
        assert_eq!(
            presented(&state),
            (Status::Ready, vec!["./a.txt".to_owned()])
        );
        bind(&mut state, "@./missing/");
        job.schedule(&mut state);
        settle(&mut job, &mut state);
        assert_eq!(presented(&state).0, Status::Unavailable);
        assert_eq!(*queries.lock().unwrap(), ["./", "./missing/"]);
    }

    #[test]
    fn stale_output_never_stages_for_another_occurrence() {
        let lister: DirectoryLister = Arc::new(|query: &str, _: usize, _: &AtomicBool| {
            thread::sleep(Duration::from_millis(20));
            Some(vec![row(&format!("{query}a"))])
        });
        let mut job = DirectoryCompletionJob::new(Some(lister));
        let mut state = State::default();
        bind(&mut state, "@./a");
        job.schedule(&mut state);
        bind(&mut state, "@./b");
        job.reconcile(&mut state, true);
        settle(&mut job, &mut state);
        assert_ne!(presented(&state).0, Status::Ready);
        job.schedule(&mut state);
        settle(&mut job, &mut state);
        assert_eq!(presented(&state), (Status::Ready, vec!["./ba".to_owned()]));
    }

    #[test]
    fn hiding_the_picker_abandons_work_and_requests_a_new_lookup() {
        let lister: DirectoryLister = Arc::new(|_: &str, _: usize, cancel: &AtomicBool| {
            while !cancel.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            Some(Vec::new())
        });
        let mut job = DirectoryCompletionJob::new(Some(lister));
        let mut state = State::default();
        bind(&mut state, "@./");
        job.schedule(&mut state);
        job.reconcile(&mut state, false);
        assert_eq!(state.directory_request, None);
        assert!(state.needs_lookup(IndexRevision::READY));
        settle(&mut job, &mut state);
        assert_eq!(presented(&state).0, Status::Loading);
        let mut unavailable = DirectoryCompletionJob::new(None);
        unavailable.schedule(&mut state);
        assert_eq!(presented(&state).0, Status::Unavailable);
    }
}
