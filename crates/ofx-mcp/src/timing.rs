use std::future::Future;
use std::time::Duration;

use ofx_contract::BoxFuture;
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

pub(crate) fn spawn(future: impl Future<Output = ()> + Send + 'static) -> JoinHandle<()> {
    spawn_task(Box::pin(future))
}

pub(crate) fn spawn_on(
    runtime: &Handle,
    future: impl Future<Output = ()> + Send + 'static,
) -> JoinHandle<()> {
    spawn_task_on(runtime, Box::pin(future))
}

fn spawn_task(task: BoxFuture<'static, ()>) -> JoinHandle<()> {
    tokio::spawn(task)
}

fn spawn_task_on(runtime: &Handle, task: BoxFuture<'static, ()>) -> JoinHandle<()> {
    runtime.spawn(task)
}
