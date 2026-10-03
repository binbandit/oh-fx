use std::future::Future;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio::time::{Instant, Sleep, Timeout};

pub(crate) fn timeout<F: Future>(duration: Duration, future: F) -> Timeout<F> {
    timeout_at(Instant::now() + duration, future)
}

pub(crate) fn timeout_at<F: Future>(deadline: Instant, future: F) -> Timeout<F> {
    tokio::time::timeout_at(deadline, future)
}

pub(crate) fn sleep(duration: Duration) -> Sleep {
    tokio::time::sleep(duration)
}

pub(crate) fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(future)
}

pub(crate) fn spawn_on<F>(runtime: &Handle, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime.spawn(future)
}
