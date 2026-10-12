use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use ofx_trace::trace_log;

use crate::composer::file_completion_state::{
    Anchor, CAPACITY, FileMatch, IndexRevision, LookupMode, State, Status,
};

pub type DirectoryLister =
    Arc<dyn Fn(&str, usize, &AtomicBool) -> Option<Vec<FileMatch>> + Send + Sync>;

const LISTER_THREAD: &str = "oh-fx-directory-completion";
const INPUT: &str = "input";
const PATH_UNAVAILABLE: &str = "PathUnavailable";

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
            trace_log!(
                INPUT,
                "directory completion cancelled request={}",
                self.request.id
            );
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
        self.drop_pending("stopped");
    }

    fn drop_pending(&mut self, reason: &str) {
        if let Some(request) = self.pending.take() {
            trace_log!(
                INPUT,
                "directory completion pending dropped request={} reason={reason}",
                request.id
            );
        }
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
            self.drop_pending(if eligible { "superseded" } else { "hidden" });
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
            fail(state, id, PATH_UNAVAILABLE);
            return;
        };
        if self.task.is_some() {
            self.pending = Some(request);
        } else if let Err(error) = self.start(request) {
            fail(state, id, error);
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
        let outcome = match task.thread.join() {
            Ok(Some(rows)) => Ok(rows),
            Ok(None) => Err(PATH_UNAVAILABLE),
            Err(_) => Err("Unexpected"),
        };
        if !task.abandoned && task.request.matches(state) {
            match outcome {
                Ok(rows) => {
                    state.directory_request = None;
                    let status = if rows.is_empty() {
                        Status::Empty
                    } else {
                        Status::Ready
                    };
                    let count = rows.len();
                    let revision = state.stage(IndexRevision::READY, status, rows);
                    trace_log!(
                        INPUT,
                        "file picker prepared revision={revision} count={count} indexed=false request={}",
                        task.request.id
                    );
                }
                Err(error) => fail(state, task.request.id, error),
            }
        } else {
            trace_log!(
                INPUT,
                "directory completion stale output dropped request={} failed={}",
                task.request.id,
                outcome.is_err()
            );
        }
        if let Some(request) = self.pending.take() {
            let id = request.id;
            if let Err(error) = self.start(request) {
                fail(state, id, error);
            }
        }
        true
    }

    fn start(&mut self, request: Request) -> Result<(), &'static str> {
        let Some(lister) = self.lister.clone() else {
            return Err(PATH_UNAVAILABLE);
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
        let thread = spawned.map_err(|error| match error.kind() {
            std::io::ErrorKind::OutOfMemory => "OutOfMemory",
            std::io::ErrorKind::WouldBlock => "SystemResources",
            _ => "Unexpected",
        })?;
        trace_log!(INPUT, "directory completion started request={}", request.id);
        self.task = Some(Task {
            request,
            done,
            cancel,
            abandoned: false,
            thread,
        });
        Ok(())
    }
}

impl Drop for DirectoryCompletionJob {
    fn drop(&mut self) {
        self.stop();
    }
}

fn fail(state: &mut State, id: u64, error: &str) {
    trace_log!(
        INPUT,
        "directory completion failed request={id} err={error}"
    );
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

    const TRACE_CHILD: &str = "OH_FX_DIRECTORY_COMPLETION_TRACE_CHILD";
    const TRACE_TEST: &str =
        "shell::directory_completion_job::tests::directory_completion_writes_its_input_lines";

    fn complete_traced_directories() {
        let lister: DirectoryLister = Arc::new(|query: &str, _: usize, cancel: &AtomicBool| {
            if query == "./slow/" {
                while !cancel.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            (query != "./missing/").then(|| vec![row(&format!("{query}secret.txt"))])
        });
        let mut job = DirectoryCompletionJob::new(Some(lister));
        let mut state = State::default();
        bind(&mut state, "@./");
        job.schedule(&mut state);
        settle(&mut job, &mut state);
        bind(&mut state, "@./missing/");
        job.schedule(&mut state);
        settle(&mut job, &mut state);
        bind(&mut state, "@./slow/");
        job.schedule(&mut state);
        job.schedule(&mut state);
        job.reconcile(&mut state, false);
        settle(&mut job, &mut state);
        DirectoryCompletionJob::new(None).schedule(&mut state);
    }

    #[test]
    fn directory_completion_writes_its_input_lines() {
        if let Some(root) = std::env::var_os(TRACE_CHILD) {
            ofx_trace::configure_from_env(std::path::Path::new(&root), None);
            complete_traced_directories();
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("trace.log");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([TRACE_TEST, "--exact", "--test-threads=1"])
            .env_clear()
            .env(TRACE_CHILD, root.path())
            .env("OH_FX_TRACE_LOG", &log)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "{status:?}");
        let written = std::fs::read_to_string(&log).unwrap_or_default();
        for line in [
            " [input] directory completion started request=1\n",
            " count=1 indexed=false request=1\n",
            " [input] directory completion failed request=2 err=PathUnavailable\n",
            " [input] directory completion pending dropped request=4 reason=hidden\n",
            " [input] directory completion cancelled request=3\n",
            " [input] directory completion stale output dropped request=3 failed=true\n",
            " [input] directory completion failed request=1 err=PathUnavailable\n",
        ] {
            assert!(written.contains(line), "{line}\n{written}");
        }
        assert!(
            written.contains(" [input] file picker prepared revision="),
            "{written}"
        );
        assert!(!written.contains("secret"), "{written}");
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
