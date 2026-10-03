use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ofx_jsonrpc::{Correlator, LineRead, LineReader, RegisterError, RequestId, WaitError};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::docker_run::{self, Cleanup};
use crate::error::McpError;
use crate::legacy_elicitation_runtime::{
    ElicitationContext, method_not_found_response, server_request_failed_response,
};
use crate::mcp_contract::validate_json_rpc_response_envelope;
use crate::protocol_messages::{build_cancellation_notification, parse_json};
use crate::timing::{spawn, spawn_on, timeout, timeout_at};
use crate::transport::{
    Cancellation, McpTransport, ProgressNotification, ProgressSink, ServerRequestPolicy,
    ShutdownMode, TransportRequest,
};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const TERMINATION_GRACE: Duration = Duration::from_secs(1);
const IMMEDIATE_DRAIN: Duration = Duration::from_millis(50);
const READER_JOIN_GRACE: Duration = Duration::from_secs(1);
const CANCELLATION_WRITE_TIMEOUT: Duration = Duration::from_millis(100);
const SERVER_REQUEST_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const STDERR_EOF_GRACE: Duration = Duration::from_millis(100);
const STDERR_HEAD_CAPACITY: usize = 1024;
const STDERR_TAIL_CAPACITY: usize = 3072;
const REJECTED_OUTPUT_CAPACITY: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StderrCapture {
    head: Vec<u8>,
    tail: Vec<u8>,
    omitted: bool,
}

impl StderrCapture {
    pub(crate) fn append(&mut self, bytes: &[u8]) {
        let head_take = bytes.len().min(STDERR_HEAD_CAPACITY - self.head.len());
        self.head.extend_from_slice(&bytes[..head_take]);
        let rest = &bytes[head_take..];
        if rest.len() >= STDERR_TAIL_CAPACITY {
            self.omitted =
                self.omitted || !self.tail.is_empty() || rest.len() > STDERR_TAIL_CAPACITY;
            self.tail.clear();
            self.tail
                .extend_from_slice(&rest[rest.len() - STDERR_TAIL_CAPACITY..]);
            return;
        }
        let overflow = (self.tail.len() + rest.len()).saturating_sub(STDERR_TAIL_CAPACITY);
        if overflow > 0 {
            self.tail.drain(..overflow);
            self.omitted = true;
        }
        self.tail.extend_from_slice(rest);
    }

    pub(crate) fn head(&self) -> &[u8] {
        &self.head
    }

    pub(crate) fn tail(&self) -> &[u8] {
        &self.tail
    }

    pub(crate) fn omitted(&self) -> bool {
        self.omitted
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RejectedOutput {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

impl RejectedOutput {
    fn new(line: &[u8]) -> Self {
        let len = line.len().min(REJECTED_OUTPUT_CAPACITY);
        Self {
            bytes: line[..len].to_vec(),
            truncated: line.len() > REJECTED_OUTPUT_CAPACITY,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChildDiagnostics {
    pub status: Option<ExitStatus>,
    pub stderr: StderrCapture,
    pub rejected_output: Option<RejectedOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StdioLaunch {
    pub(crate) argv: Vec<String>,
    pub(crate) environment: Vec<(String, String)>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) initial_max_frame_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopMode {
    Graceful,
    Forced,
    Immediate,
    Abandon,
}

impl From<ShutdownMode> for StopMode {
    fn from(mode: ShutdownMode) -> Self {
        match mode {
            ShutdownMode::Graceful => Self::Graceful,
            ShutdownMode::Immediate => Self::Immediate,
            ShutdownMode::ProcessExit => Self::Abandon,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionState {
    Running,
    Failed,
    Stopping,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum WritePhase {
    Waiting = 0,
    Writing = 1,
    Committed = 2,
    Failed = 3,
}

struct PendingMeta {
    max_frame_bytes: usize,
    progress: Option<ProgressSink>,
    elicitation: Option<Arc<ElicitationContext>>,
}

#[derive(Debug, PartialEq)]
enum ResponseId {
    Integer(u64),
    String,
}

#[derive(Debug, PartialEq)]
enum ProgressToken {
    Integer(u64),
    String,
}

#[derive(Debug, PartialEq)]
enum Inbound {
    Response(ResponseId),
    ProgressNotification(ProgressToken, ProgressNotification),
    Notification,
    Cancelled,
    Request,
}

pub(crate) struct Shared {
    pid: u32,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    responses: Correlator<String, McpError>,
    pending: Mutex<HashMap<u64, PendingMeta>>,
    max_frame_bytes: AtomicUsize,
    next_request_id: AtomicU64,
    state: Mutex<ConnectionState>,
    child_reaped: AtomicBool,
    reader_done: watch::Sender<bool>,
    stderr_done: watch::Sender<bool>,
    notifications: mpsc::UnboundedSender<Value>,
    diagnostics: Mutex<ChildDiagnostics>,
}

pub(crate) struct StdioDispatcher {
    shared: Arc<Shared>,
    reader: Mutex<Option<JoinHandle<()>>>,
    stderr: Mutex<Option<JoinHandle<()>>>,
    docker_cleanup: Mutex<Option<Cleanup>>,
}

impl StdioDispatcher {
    pub(crate) fn spawn(
        launch: StdioLaunch,
        notifications: mpsc::UnboundedSender<Value>,
    ) -> Result<Self, McpError> {
        let prepared = docker_run::prepare(launch.argv)?;
        let (program, args) = prepared
            .argv
            .split_first()
            .ok_or(McpError::McpInvalidServerConfig)?;
        let mut command = Command::new(program);
        command
            .args(args)
            .envs(launch.environment.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        if let Some(cwd) = &launch.cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn()?;
        let pid = child.id().ok_or(McpError::McpConnectionClosed)?;
        let stdin = child.stdin.take().ok_or(McpError::McpConnectionClosed)?;
        let stdout = child.stdout.take().ok_or(McpError::McpConnectionClosed)?;
        let stderr = child.stderr.take();
        let (reader_done, _) = watch::channel(false);
        let (stderr_done, _) = watch::channel(stderr.is_none());
        let shared = Arc::new(Shared {
            pid,
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            responses: Correlator::new(),
            pending: Mutex::new(HashMap::new()),
            max_frame_bytes: AtomicUsize::new(launch.initial_max_frame_bytes),
            next_request_id: AtomicU64::new(0),
            state: Mutex::new(ConnectionState::Running),
            child_reaped: AtomicBool::new(false),
            reader_done,
            stderr_done,
            notifications,
            diagnostics: Mutex::new(ChildDiagnostics::default()),
        });
        let stderr_task = stderr.map(|stderr| spawn(drain_stderr(Arc::clone(&shared), stderr)));
        let reader = spawn(reader_main(Arc::clone(&shared), stdout, child));
        Ok(Self {
            shared,
            reader: Mutex::new(Some(reader)),
            stderr: Mutex::new(stderr_task),
            docker_cleanup: Mutex::new(
                prepared
                    .cleanup
                    .map(|cleanup| cleanup.with_environment(launch.environment)),
            ),
        })
    }

    pub(crate) fn child_diagnostics(&self) -> ChildDiagnostics {
        lock(&self.shared.diagnostics).clone()
    }

    pub(crate) async fn settled_diagnostics(&self) -> ChildDiagnostics {
        wait_until_set(&self.shared.reader_done, STDERR_EOF_GRACE * 2).await;
        wait_until_set(&self.shared.stderr_done, STDERR_EOF_GRACE).await;
        self.child_diagnostics()
    }

    async fn run_request(&self, request: TransportRequest) -> Result<String, McpError> {
        let shared = &self.shared;
        shared.ensure_running()?;
        let id = request.id;
        let key = request_key(id);
        let pending = shared
            .responses
            .register(key)
            .map_err(|error| match error {
                RegisterError::Duplicate => McpError::McpDuplicateRequestId,
                RegisterError::Closed(_) => McpError::McpConnectionClosed,
            })?;
        lock(&shared.pending).insert(
            id,
            PendingMeta {
                max_frame_bytes: request.max_response_bytes,
                progress: request.progress.clone(),
                elicitation: (request.server_requests == ServerRequestPolicy::RefuseElicitation)
                    .then(|| Arc::new(ElicitationContext::default())),
            },
        );
        shared
            .max_frame_bytes
            .fetch_max(request.max_response_bytes, Ordering::Relaxed);
        let _meta = PendingMetaGuard { shared, id };
        let phase = AtomicU8::new(WritePhase::Waiting as u8);
        let mut cancel_on_drop = CancelOnDrop {
            shared: Arc::clone(shared),
            id,
            phase: &phase,
            armed: request.send_cancellation,
        };
        shared
            .write_bounded(&request.body, request.deadline, &phase)
            .await?;
        let outcome = pending.wait_until(request.deadline).await;
        cancel_on_drop.armed = false;
        match outcome {
            Ok(frame) => Ok(frame),
            Err(WaitError::TimedOut) => {
                if request.send_cancellation {
                    shared.send_cancellation(id, "McpRequestTimedOut").await;
                }
                Err(McpError::McpRequestTimedOut)
            }
            Err(WaitError::Failed(error)) => Err(error),
            Err(WaitError::Abandoned) => Err(McpError::McpConnectionClosed),
        }
    }

    pub(crate) async fn stop(&self, mode: StopMode) {
        let shared = &self.shared;
        {
            let mut state = lock(&shared.state);
            if matches!(*state, ConnectionState::Running | ConnectionState::Failed) {
                *state = ConnectionState::Stopping;
            }
        }
        shared.responses.close(McpError::McpConnectionClosed);
        shared.close_stdin().await;
        match mode {
            StopMode::Graceful => wait_until_set(&shared.reader_done, SHUTDOWN_GRACE).await,
            StopMode::Immediate => wait_until_set(&shared.reader_done, IMMEDIATE_DRAIN).await,
            StopMode::Forced | StopMode::Abandon => {}
        }
        if matches!(mode, StopMode::Graceful | StopMode::Forced) && shared.child_may_be_running() {
            terminate_child_gracefully(shared.pid);
            wait_until_set(&shared.reader_done, TERMINATION_GRACE).await;
        }
        shared.kill_child();
        let reader = lock(&self.reader).take();
        if let Some(mut reader) = reader
            && timeout(READER_JOIN_GRACE, &mut reader).await.is_err()
        {
            reader.abort();
        }
        kill_process_group(shared.pid);
        let stderr = lock(&self.stderr).take();
        if let Some(mut stderr) = stderr
            && timeout(STDERR_EOF_GRACE, &mut stderr).await.is_err()
        {
            stderr.abort();
        }
        *lock(&shared.state) = ConnectionState::Stopped;
        let cleanup = lock(&self.docker_cleanup).take();
        if let Some(cleanup) = cleanup {
            cleanup.run().await;
        }
    }
}

impl Drop for StdioDispatcher {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.shared.state);
            if *state == ConnectionState::Stopped {
                return;
            }
            *state = ConnectionState::Stopping;
        }
        self.shared.responses.close(McpError::McpConnectionClosed);
        self.shared.kill_child();
        kill_process_group(self.shared.pid);
        let cleanup = lock(&self.docker_cleanup).take();
        if let (Some(cleanup), Ok(runtime)) = (cleanup, tokio::runtime::Handle::try_current()) {
            spawn_on(&runtime, cleanup.run());
        }
    }
}

impl McpTransport for StdioDispatcher {
    fn next_request_id(&self) -> Result<u64, McpError> {
        self.shared.ensure_running()?;
        let id = self.shared.next_request_id.fetch_add(1, Ordering::Relaxed);
        if id == u64::MAX {
            return Err(McpError::McpRequestIdExhausted);
        }
        Ok(id)
    }

    fn request(
        &self,
        request: TransportRequest,
    ) -> impl Future<Output = Result<String, McpError>> + Send {
        self.run_request(request)
    }

    async fn notify(&self, body: String, deadline: Instant) -> Result<(), McpError> {
        let phase = AtomicU8::new(WritePhase::Waiting as u8);
        self.shared.write_bounded(&body, deadline, &phase).await
    }

    fn is_running(&self) -> bool {
        *lock(&self.shared.state) == ConnectionState::Running
    }

    fn shutdown(&self, mode: ShutdownMode) -> impl Future<Output = ()> + Send {
        self.stop(mode.into())
    }
}

impl Shared {
    fn ensure_running(&self) -> Result<(), McpError> {
        if *lock(&self.state) == ConnectionState::Running {
            Ok(())
        } else {
            Err(McpError::McpConnectionClosed)
        }
    }

    fn is_stopping(&self) -> bool {
        matches!(
            *lock(&self.state),
            ConnectionState::Stopping | ConnectionState::Stopped
        )
    }

    fn child_may_be_running(&self) -> bool {
        !self.child_reaped.load(Ordering::Acquire)
    }

    fn fail_connection(&self, error: McpError) {
        {
            let mut state = lock(&self.state);
            if matches!(*state, ConnectionState::Stopping | ConnectionState::Stopped) {
                return;
            }
            *state = ConnectionState::Failed;
        }
        self.responses.close(error);
    }

    fn kill_child(&self) {
        if self.child_may_be_running() {
            terminate_child(self.pid);
        }
    }

    async fn close_stdin(&self) {
        let mut stdin = if let Ok(stdin) = self.stdin.try_lock() {
            stdin
        } else {
            self.kill_child();
            self.stdin.lock().await
        };
        stdin.take();
    }

    async fn write_bounded(
        &self,
        body: &str,
        deadline: Instant,
        phase: &AtomicU8,
    ) -> Result<(), McpError> {
        let taint = WriteTaint {
            shared: self,
            phase,
        };
        let outcome = timeout_at(deadline, self.write_frame(body, phase)).await;
        drop(taint);
        match outcome {
            Err(_) => Err(McpError::McpRequestTimedOut),
            Ok(Err(error)) => {
                if load_phase(phase) != WritePhase::Waiting {
                    self.fail_connection(error.clone());
                    self.kill_child();
                }
                Err(error)
            }
            Ok(Ok(())) => Ok(()),
        }
    }

    async fn write_frame(&self, body: &str, phase: &AtomicU8) -> Result<(), McpError> {
        let mut stdin = self.stdin.lock().await;
        let pipe = stdin.as_mut().ok_or(McpError::McpConnectionClosed)?;
        phase.store(WritePhase::Writing as u8, Ordering::Release);
        let mut frame = Vec::with_capacity(body.len() + 1);
        frame.extend_from_slice(body.as_bytes());
        frame.push(b'\n');
        let written = match pipe.write_all(&frame).await {
            Ok(()) => pipe.flush().await,
            Err(error) => Err(error),
        };
        let next = if written.is_ok() {
            WritePhase::Committed
        } else {
            WritePhase::Failed
        };
        phase.store(next as u8, Ordering::Release);
        written.map_err(write_error)
    }

    pub(crate) async fn send_cancellation(&self, request_id: u64, reason: &str) {
        let body = build_cancellation_notification(request_id, reason);
        let phase = AtomicU8::new(WritePhase::Waiting as u8);
        let _ = self
            .write_bounded(&body, Instant::now() + CANCELLATION_WRITE_TIMEOUT, &phase)
            .await;
    }

    fn record_rejected_output(&self, line: &[u8]) {
        lock(&self.diagnostics).rejected_output = Some(RejectedOutput::new(line));
    }

    fn dispatch_frame(self: &Arc<Self>, frame: Vec<u8>) -> Result<(), McpError> {
        let Some(value) = parse_json(&frame) else {
            self.record_rejected_output(&frame);
            return Err(McpError::McpInvalidJson);
        };
        let inbound = match classify_inbound(&value) {
            Ok(inbound) => inbound,
            Err(error) => {
                if error == McpError::McpInvalidJson {
                    self.record_rejected_output(&frame);
                }
                return Err(error);
            }
        };
        match inbound {
            Inbound::Response(ResponseId::Integer(id)) => self.deliver_response(id, frame),
            Inbound::ProgressNotification(ProgressToken::Integer(id), progress) => {
                let sink = lock(&self.pending)
                    .get(&id)
                    .and_then(|meta| meta.progress.clone());
                if let Some(sink) = sink {
                    sink(progress);
                }
            }
            Inbound::Notification => {
                let _ = self.notifications.send(value);
            }
            Inbound::Request => self.dispatch_server_request(frame),
            Inbound::Response(ResponseId::String)
            | Inbound::ProgressNotification(ProgressToken::String, _)
            | Inbound::Cancelled => {}
        }
        Ok(())
    }

    fn deliver_response(&self, id: u64, frame: Vec<u8>) {
        let Some(max_frame_bytes) = lock(&self.pending)
            .get(&id)
            .map(|meta| meta.max_frame_bytes)
        else {
            return;
        };
        let key = request_key(id);
        if frame.len() > max_frame_bytes {
            self.responses
                .resolve(&key, Err(McpError::McpResponseFrameTooLarge));
            self.fail_connection(McpError::McpResponseFrameTooLarge);
            self.kill_child();
            return;
        }
        let text = String::from_utf8(frame).map_err(|_| McpError::McpInvalidJson);
        self.responses.resolve(&key, text);
    }

    fn dispatch_server_request(self: &Arc<Self>, frame: Vec<u8>) {
        let owner = {
            let pending = lock(&self.pending);
            let mut owners = pending.values().filter_map(|meta| meta.elicitation.clone());
            match (owners.next(), owners.next()) {
                (Some(owner), None) => Some(owner),
                _ => None,
            }
        };
        let shared = Arc::clone(self);
        spawn(async move {
            let response = match owner {
                Some(context) => context
                    .respond(&frame)
                    .unwrap_or_else(|_| server_request_failed_response(&frame)),
                None => method_not_found_response(&frame),
            };
            let phase = AtomicU8::new(WritePhase::Waiting as u8);
            let _ = shared
                .write_bounded(
                    &response,
                    Instant::now() + SERVER_REQUEST_WRITE_TIMEOUT,
                    &phase,
                )
                .await;
        });
    }
}

async fn wait_until_set(flag: &watch::Sender<bool>, limit: Duration) {
    let mut set = flag.subscribe();
    let _ = timeout(limit, set.wait_for(|set| *set)).await;
}

struct PendingMetaGuard<'a> {
    shared: &'a Shared,
    id: u64,
}

impl Drop for PendingMetaGuard<'_> {
    fn drop(&mut self) {
        lock(&self.shared.pending).remove(&self.id);
    }
}

struct WriteTaint<'a> {
    shared: &'a Shared,
    phase: &'a AtomicU8,
}

impl Drop for WriteTaint<'_> {
    fn drop(&mut self) {
        if load_phase(self.phase) == WritePhase::Writing {
            self.shared.fail_connection(McpError::McpWriteInterrupted);
            self.shared.kill_child();
        }
    }
}

struct CancelOnDrop<'a> {
    shared: Arc<Shared>,
    id: u64,
    phase: &'a AtomicU8,
    armed: bool,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if !self.armed || load_phase(self.phase) != WritePhase::Committed {
            return;
        }
        Cancellation::Stdio(Arc::clone(&self.shared)).send_in_background(self.id);
    }
}

async fn reader_main(shared: Arc<Shared>, stdout: ChildStdout, mut child: Child) {
    let mut reader = LineReader::new(BufReader::new(stdout));
    let terminal = loop {
        match reader
            .read_line(|| shared.max_frame_bytes.load(Ordering::Relaxed))
            .await
        {
            Ok(Some(LineRead::Line(mut line))) => {
                while line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    continue;
                }
                if let Err(error) = shared.dispatch_frame(line) {
                    break error;
                }
            }
            Ok(Some(LineRead::Overflow)) => break McpError::McpResponseFrameTooLarge,
            Ok(Some(LineRead::Incomplete)) => break McpError::McpIncompleteBody,
            Ok(None) => break McpError::McpConnectionClosed,
            Err(error) => break McpError::from(error),
        }
    };
    if !shared.is_stopping() {
        shared.fail_connection(terminal);
        shared.kill_child();
    }
    let status = child.wait().await.ok();
    shared.child_reaped.store(true, Ordering::Release);
    lock(&shared.diagnostics).status = status;
    shared.reader_done.send_replace(true);
}

async fn drain_stderr(shared: Arc<Shared>, mut stderr: ChildStderr) {
    let mut buffer = [0_u8; 4096];
    while let Ok(count) = stderr.read(&mut buffer).await {
        if count == 0 {
            break;
        }
        lock(&shared.diagnostics)
            .stderr
            .append(buffer.get(..count).unwrap_or_default());
    }
    shared.stderr_done.send_replace(true);
}

fn classify_inbound(value: &Value) -> Result<Inbound, McpError> {
    let object = value.as_object().ok_or(McpError::McpInvalidJson)?;
    if let Some(method) = object.get("method") {
        let method = method.as_str().ok_or(McpError::McpInvalidJson)?;
        if object.contains_key("id") {
            return Ok(Inbound::Request);
        }
        if method == "notifications/progress" {
            let params = object.get("params").ok_or(McpError::McpInvalidProgress)?;
            let (token, progress) = parse_progress(params)?;
            return Ok(Inbound::ProgressNotification(token, progress));
        }
        if method == "notifications/cancelled" && parse_cancelled_request_id(object.get("params")) {
            return Ok(Inbound::Cancelled);
        }
        return Ok(Inbound::Notification);
    }
    validate_json_rpc_response_envelope(value)?;
    match object.get("id") {
        Some(Value::Number(number)) => number
            .as_u64()
            .map(|id| Inbound::Response(ResponseId::Integer(id)))
            .ok_or(McpError::McpUnsupportedResponseId),
        Some(Value::String(_)) => Ok(Inbound::Response(ResponseId::String)),
        Some(_) => Err(McpError::McpUnsupportedResponseId),
        None => Err(McpError::McpInvalidJson),
    }
}

fn parse_cancelled_request_id(params: Option<&Value>) -> bool {
    params
        .and_then(|params| params.get("requestId"))
        .and_then(Value::as_u64)
        .is_some()
}

fn parse_progress(value: &Value) -> Result<(ProgressToken, ProgressNotification), McpError> {
    let object = value.as_object().ok_or(McpError::McpInvalidProgress)?;
    let token = match object.get("progressToken") {
        Some(Value::Number(number)) => {
            ProgressToken::Integer(number.as_u64().ok_or(McpError::McpInvalidProgress)?)
        }
        Some(Value::String(_)) => ProgressToken::String,
        _ => return Err(McpError::McpInvalidProgress),
    };
    let progress = json_number(object.get("progress"))?;
    let total = match object.get("total") {
        Some(total) => Some(json_number(Some(total))?),
        None => None,
    };
    let message = match object.get("message") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(McpError::McpInvalidProgress),
        None => None,
    };
    Ok((
        token,
        ProgressNotification {
            progress,
            total,
            message,
        },
    ))
}

fn json_number(value: Option<&Value>) -> Result<f64, McpError> {
    value
        .and_then(Value::as_f64)
        .ok_or(McpError::McpInvalidProgress)
}

fn request_key(id: u64) -> RequestId {
    i64::try_from(id).map_or_else(|_| RequestId::String(id.to_string()), RequestId::Integer)
}

fn load_phase(phase: &AtomicU8) -> WritePhase {
    match phase.load(Ordering::Acquire) {
        0 => WritePhase::Waiting,
        1 => WritePhase::Writing,
        2 => WritePhase::Committed,
        _ => WritePhase::Failed,
    }
}

fn write_error(error: std::io::Error) -> McpError {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        McpError::McpConnectionClosed
    } else {
        McpError::from(error)
    }
}

fn terminate_child(pid: u32) {
    let Some(pid) = rustix_pid(pid) else {
        return;
    };
    if rustix::process::kill_process_group(pid, rustix::process::Signal::KILL).is_err() {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
}

fn kill_process_group(pid: u32) {
    if let Some(pid) = rustix_pid(pid) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

fn terminate_child_gracefully(pid: u32) {
    if let Some(pid) = rustix_pid(pid) {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
}

fn rustix_pid(pid: u32) -> Option<rustix::process::Pid> {
    i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn shell(script: &str) -> (StdioDispatcher, mpsc::UnboundedReceiver<Value>) {
        shell_with_limit(script, 1024 * 1024)
    }

    fn shell_with_limit(
        script: &str,
        initial_max_frame_bytes: usize,
    ) -> (StdioDispatcher, mpsc::UnboundedReceiver<Value>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let dispatcher = StdioDispatcher::spawn(
            StdioLaunch {
                argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
                environment: Vec::new(),
                cwd: None,
                initial_max_frame_bytes,
            },
            sender,
        )
        .unwrap();
        (dispatcher, receiver)
    }

    async fn stderr_eventually_contains(dispatcher: &StdioDispatcher, needle: &str) -> bool {
        for _ in 0..100 {
            let stderr = dispatcher.child_diagnostics().stderr;
            if String::from_utf8_lossy(stderr.head()).contains(needle) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    fn request(dispatcher: &StdioDispatcher, method: &str, deadline: Duration) -> TransportRequest {
        let id = dispatcher.next_request_id().unwrap();
        let body =
            format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"params\":{{}}}}");
        TransportRequest::new(id, body, 1024 * 1024, Instant::now() + deadline)
    }

    const ECHO_RESULT: &str = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  if [ -n "$id" ]; then printf '{"jsonrpc":"2.0","id":%s,"result":{"echo":%s}}\n' "$id" "$id"; fi
done"#;

    #[test]
    fn classify_inbound_separates_responses_notifications_progress_and_requests() {
        let cases = [
            (
                json!({"jsonrpc":"2.0","id":8,"result":{}}),
                Inbound::Response(ResponseId::Integer(8)),
            ),
            (
                json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}),
                Inbound::Notification,
            ),
            (
                json!({"jsonrpc":"2.0","id":9,"method":"sampling/createMessage","params":{}}),
                Inbound::Request,
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(classify_inbound(&value), Ok(expected));
        }
        let progress = json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":8,"progress":2,"total":4,"message":"half"}});
        assert!(matches!(
            classify_inbound(&progress),
            Ok(Inbound::ProgressNotification(ProgressToken::Integer(8), _))
        ));
    }

    #[test]
    fn classify_inbound_preserves_request_specific_progress_values() {
        let value = json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":42,"progress":1.5,"total":3,"message":"working"}});
        assert_eq!(
            classify_inbound(&value),
            Ok(Inbound::ProgressNotification(
                ProgressToken::Integer(42),
                ProgressNotification {
                    progress: 1.5,
                    total: Some(3.0),
                    message: Some("working".to_owned()),
                }
            ))
        );
    }

    #[test]
    fn classify_inbound_accepts_string_response_ids_and_rejects_malformed_progress() {
        assert_eq!(
            classify_inbound(&json!({"jsonrpc":"2.0","id":"response","result":{}})),
            Ok(Inbound::Response(ResponseId::String))
        );
        for value in [
            json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1}}),
            json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":1,"progress":"one"}}),
        ] {
            assert_eq!(classify_inbound(&value), Err(McpError::McpInvalidProgress));
        }
    }

    #[test]
    fn classify_inbound_rejects_malformed_response_envelopes() {
        for value in [
            json!({"id":1,"result":{}}),
            json!({"jsonrpc":"1.0","id":1,"result":{}}),
            json!({"jsonrpc":"2.0","id":1}),
            json!({"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"ambiguous"}}),
        ] {
            assert_eq!(classify_inbound(&value), Err(McpError::McpInvalidJson));
        }
    }

    #[test]
    fn classify_inbound_treats_unidentified_cancellations_as_notifications() {
        assert_eq!(
            classify_inbound(
                &json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3}})
            ),
            Ok(Inbound::Cancelled)
        );
        assert_eq!(
            classify_inbound(
                &json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"x"}})
            ),
            Ok(Inbound::Notification)
        );
    }

    #[test]
    fn stderr_capture_keeps_the_leading_bytes_and_the_newest_trailing_bytes() {
        let mut capture = StderrCapture::default();
        capture.append(&[b'h'; STDERR_HEAD_CAPACITY]);
        capture.append(b"middle");
        assert!(!capture.omitted());
        capture.append(&[b't'; STDERR_TAIL_CAPACITY]);
        assert!(capture.omitted());
        assert_eq!(capture.head(), &[b'h'; STDERR_HEAD_CAPACITY][..]);
        assert_eq!(capture.tail(), &[b't'; STDERR_TAIL_CAPACITY][..]);
        let mut small = StderrCapture::default();
        small.append(b"short");
        assert_eq!(small.head(), b"short");
        assert!(small.tail().is_empty());
    }

    #[test]
    fn rejected_stdout_keeps_a_bounded_prefix_of_the_line() {
        let rejected = RejectedOutput::new(&[b'x'; 300]);
        assert_eq!(rejected.bytes.len(), REJECTED_OUTPUT_CAPACITY);
        assert!(rejected.truncated);
    }

    #[tokio::test]
    async fn one_dispatcher_keeps_reversed_concurrent_responses_with_their_requests() {
        let script = r#"read -r first; read -r second
printf '{"jsonrpc":"2.0","id":1,"result":{"order":"second"}}\n'
printf '{"jsonrpc":"2.0","id":0,"result":{"order":"first"}}\n'
cat >/dev/null"#;
        let (dispatcher, _notifications) = shell(script);
        let first = request(&dispatcher, "a", Duration::from_secs(5));
        let second = request(&dispatcher, "b", Duration::from_secs(5));
        let (first, second) = tokio::join!(dispatcher.request(first), dispatcher.request(second));
        assert!(first.unwrap().contains("\"first\""));
        assert!(second.unwrap().contains("\"second\""));
        dispatcher.shutdown(ShutdownMode::Graceful).await;
    }

    #[tokio::test]
    async fn routes_notifications_and_progress_and_ignores_unknown_responses() {
        let script = r#"read -r line
printf '{"jsonrpc":"2.0","id":99,"result":{}}\n'
printf '{"jsonrpc":"2.0","id":"text","result":{}}\n'
printf '{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":0,"progress":1,"total":2}}\n'
printf '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}\n'
printf '{"jsonrpc":"2.0","id":0,"result":{}}\n'
cat >/dev/null"#;
        let (dispatcher, mut notifications) = shell(script);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut call = request(&dispatcher, "tools/call", Duration::from_secs(5));
        let sink_seen = Arc::clone(&seen);
        call.progress = Some(Arc::new(move |progress| lock(&sink_seen).push(progress)));
        assert_eq!(
            dispatcher.request(call).await.unwrap(),
            "{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{}}"
        );
        assert_eq!(
            *lock(&seen),
            vec![ProgressNotification {
                progress: 1.0,
                total: Some(2.0),
                message: None,
            }]
        );
        assert_eq!(
            notifications.recv().await.unwrap()["method"],
            "notifications/tools/list_changed"
        );
        dispatcher.shutdown(ShutdownMode::Graceful).await;
    }

    #[tokio::test]
    async fn answers_unowned_server_requests_with_method_not_found() {
        let script = r#"printf '{"jsonrpc":"2.0","id":5,"method":"roots/list","params":{}}\n'
while IFS= read -r line; do
  case "$line" in
    *'"error"'*) printf '%s\n' "$line" >&2 ;;
    *'"method"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
  esac
done"#;
        let (dispatcher, _notifications) = shell(script);
        let call = request(&dispatcher, "tools/list", Duration::from_secs(5));
        dispatcher.request(call).await.unwrap();
        assert!(
            stderr_eventually_contains(
                &dispatcher,
                "{\"jsonrpc\":\"2.0\",\"id\":5,\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}"
            )
            .await
        );
        dispatcher.shutdown(ShutdownMode::Graceful).await;
    }

    #[tokio::test]
    async fn operation_timeout_sends_a_cancellation_and_shutdown_joins_an_uncooperative_child() {
        let script = r#"trap '' TERM
read -r line
read -r cancellation
printf '%s\n' "$cancellation" >&2
while :; do sleep 1; done"#;
        let (dispatcher, _notifications) = shell(script);
        let mut call = request(&dispatcher, "tools/call", Duration::from_millis(200));
        call.send_cancellation = true;
        assert_eq!(
            dispatcher.request(call).await,
            Err(McpError::McpRequestTimedOut)
        );
        assert!(
            stderr_eventually_contains(
                &dispatcher,
                "\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":0,\"reason\":\"McpRequestTimedOut\"}"
            )
            .await
        );
        let started = std::time::Instant::now();
        let pid = dispatcher.shared.pid;
        dispatcher.shutdown(ShutdownMode::Graceful).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(rustix::process::test_kill_process(rustix_pid(pid).unwrap()).is_err());
    }

    #[tokio::test]
    async fn mcp_stdio_keeps_the_stdout_line_it_rejected_as_not_an_mcp_message() {
        let (dispatcher, _notifications) = shell("printf 'Server banner v1\\n'; cat >/dev/null");
        let call = request(&dispatcher, "initialize", Duration::from_secs(5));
        assert_eq!(
            dispatcher.request(call).await,
            Err(McpError::McpInvalidJson)
        );
        let diagnostics = dispatcher.settled_diagnostics().await;
        assert_eq!(
            diagnostics.rejected_output.map(|output| output.bytes),
            Some(b"Server banner v1".to_vec())
        );
        dispatcher.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn mcp_stdio_records_how_a_child_that_exits_before_replying_ended_and_what_it_printed() {
        let (dispatcher, _notifications) = shell("echo 'fatal: missing token' >&2; exit 3");
        let call = request(&dispatcher, "initialize", Duration::from_secs(5));
        let outcome = dispatcher.request(call).await;
        assert!(matches!(
            outcome,
            Err(McpError::McpConnectionClosed | McpError::McpWriteInterrupted)
        ));
        let diagnostics = dispatcher.settled_diagnostics().await;
        assert_eq!(diagnostics.status.and_then(|status| status.code()), Some(3));
        assert_eq!(diagnostics.stderr.head(), b"fatal: missing token\n");
        assert!(!dispatcher.is_running());
        dispatcher.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn oversized_frames_fail_the_connection() {
        let script = r"read -r line
head -c 2048 /dev/zero | tr '\0' 'x'
printf '\n'
cat >/dev/null";
        let (dispatcher, _notifications) = shell_with_limit(script, 1024);
        let id = dispatcher.next_request_id().unwrap();
        let body = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"x\"}}");
        let call = TransportRequest::new(id, body, 1024, Instant::now() + Duration::from_secs(5));
        assert_eq!(
            dispatcher.request(call).await,
            Err(McpError::McpResponseFrameTooLarge)
        );
        assert!(!dispatcher.is_running());
        dispatcher.shutdown(ShutdownMode::Immediate).await;
    }

    #[tokio::test]
    async fn requests_after_shutdown_report_a_closed_connection() {
        let (dispatcher, _notifications) = shell(ECHO_RESULT);
        let call = request(&dispatcher, "ping", Duration::from_secs(5));
        assert!(
            dispatcher
                .request(call.clone())
                .await
                .unwrap()
                .contains("\"echo\":0")
        );
        dispatcher.stop(StopMode::Graceful).await;
        assert_eq!(
            dispatcher.request(call).await,
            Err(McpError::McpConnectionClosed)
        );
        assert_eq!(
            dispatcher.next_request_id(),
            Err(McpError::McpConnectionClosed)
        );
    }

    #[tokio::test]
    async fn dropping_the_dispatcher_kills_the_server_without_grace() {
        let (dispatcher, _notifications) = shell("trap '' TERM; while :; do sleep 1; done");
        let mut reader_done = dispatcher.shared.reader_done.subscribe();
        drop(dispatcher);
        assert!(
            timeout(Duration::from_secs(2), reader_done.wait_for(|done| *done))
                .await
                .is_ok()
        );
    }

    fn process_ended(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat
                .rsplit_once(") ")
                .is_some_and(|(_, fields)| fields.starts_with('Z')),
            Err(_) => true,
        }
    }

    #[tokio::test]
    async fn stopping_kills_what_the_server_left_in_its_process_group() {
        let state = tempfile::tempdir().unwrap();
        let pid_file = state.path().join("sleeper");
        let script = format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > '{}'; read -r line; exit 0",
            pid_file.display()
        );
        let (dispatcher, _notifications) = shell(&script);
        let mut sleeper = None;
        for _ in 0..200 {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            {
                sleeper = Some(pid);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let sleeper = sleeper.unwrap();
        dispatcher.shutdown(ShutdownMode::Graceful).await;
        let mut ended = false;
        for _ in 0..200 {
            if process_ended(sleeper) {
                ended = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ended, "the server's background process outlived shutdown");
    }

    #[tokio::test]
    async fn dropping_a_committed_request_sends_a_cancellation() {
        let script = r#"read -r line
read -r cancellation
printf '%s\n' "$cancellation" >&2
cat >/dev/null"#;
        let (dispatcher, _notifications) = shell(script);
        let mut call = request(&dispatcher, "tools/call", Duration::from_secs(5));
        call.send_cancellation = true;
        let outcome = timeout(Duration::from_millis(200), dispatcher.request(call)).await;
        assert!(outcome.is_err());
        assert!(stderr_eventually_contains(&dispatcher, "\"reason\":\"Cancelled\"").await);
        dispatcher.shutdown(ShutdownMode::Graceful).await;
    }
}
