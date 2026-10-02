use std::fmt;
use std::io::{self, IsTerminal};
use std::panic;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use ofx_cli::LaunchModifiers;
use ofx_contract::{Notice, NoticeTone, PermissionMode, UiCommand, UiEvent};
use ofx_exec::{ManagedExecutions, SessionSupervisor};
use ofx_tui::{ShellOptions, TerminalError, UiEventSender, run_shell, ui_channel};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

use crate::app_agent_runtime::Controller;
use crate::app_bootstrap_runtime::{AgentSetup, Launch, Profile, ProfileError};
use crate::app_commands::slash_command_specs;
use crate::app_panic_runtime::PanicCapture;
use crate::app_upgrade_runtime;
use crate::codex_provider::SubscriptionEndpoints;

const WORKER_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const WORKER_THREAD: &str = "oh-fx-agent";

const SELF_EXE_NOT_FOUND: &str = "SelfExeNotFound";

struct Session {
    profile: Profile,
    setup: AgentSetup,
    executions: ManagedExecutions,
    permission_mode: PermissionMode,
}

pub fn run_interactive(modifiers: &LaunchModifiers) -> ExitCode {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("{}", TerminalError::NotATerminal);
        return ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{}", failure_line(&error));
            return ExitCode::FAILURE;
        }
    };
    let session = match runtime.block_on(bootstrap(modifiers)) {
        Ok(session) => session,
        Err(lines) => {
            for line in lines {
                eprintln!("{line}");
            }
            return ExitCode::FAILURE;
        }
    };
    let update = app_upgrade_runtime::announce_and_schedule();
    match run(session, update, runtime) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            if matches!(
                error,
                SessionError::Terminal(TerminalError::TerminalTooSmall)
            ) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

async fn bootstrap(modifiers: &LaunchModifiers) -> Result<Session, Vec<String>> {
    let profile = Profile::load().map_err(|error| profile_failure_lines(&error))?;
    let supervisor = SessionSupervisor::current_executable()
        .map_err(|_| vec![failure_line(&SELF_EXE_NOT_FOUND)])?;
    let executions = ManagedExecutions::new(supervisor);
    let settings = profile.settings();
    let permission_mode = settings.permission_mode();
    let setup = profile
        .connect_interactive(
            Launch {
                model: modifiers.model(),
                permission_mode,
                system_prompt: None,
                reasoning_effort: modifiers
                    .reasoning_effort()
                    .cloned()
                    .unwrap_or_else(|| settings.reasoning_effort())
                    .into_named(),
                fast_mode: modifiers
                    .fast_mode()
                    .unwrap_or_else(|| settings.fast_mode()),
                context_limits: modifiers.context_limit_overrides(),
                command_timeout: None,
                executions: &executions,
                endpoints: SubscriptionEndpoints::default(),
            },
            &CancellationToken::new(),
        )
        .await
        .map_err(|error| vec![failure_line(&error)])?;
    Ok(Session {
        profile,
        setup,
        executions,
        permission_mode,
    })
}

fn failure_line(error: &dyn fmt::Display) -> String {
    format!("oh-fx: {error}")
}

fn profile_failure_lines(error: &ProfileError) -> Vec<String> {
    let mut lines: Vec<String> = match error {
        ProfileError::Unusable(diagnostics) => diagnostics
            .iter()
            .map(|diagnostic| failure_line(diagnostic))
            .collect(),
        ProfileError::WorkspaceUnavailable | ProfileError::Settings(_) => Vec::new(),
    };
    lines.push(failure_line(error));
    lines
}

#[derive(Debug)]
enum SessionError {
    Terminal(TerminalError),
    AgentStopped(Option<String>),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Terminal(
                error @ (TerminalError::NotATerminal | TerminalError::TerminalTooSmall),
            ) => write!(formatter, "{error}"),
            Self::Terminal(error) => write!(formatter, "oh-fx: {error}"),
            Self::AgentStopped(Some(report)) => {
                write!(formatter, "oh-fx: the agent stopped unexpectedly: {report}")
            }
            Self::AgentStopped(None) => write!(formatter, "oh-fx: the agent stopped unexpectedly"),
        }
    }
}

impl From<TerminalError> for SessionError {
    fn from(error: TerminalError) -> Self {
        Self::Terminal(error)
    }
}

impl From<io::Error> for SessionError {
    fn from(error: io::Error) -> Self {
        Self::Terminal(error.into())
    }
}

fn run(session: Session, update: Option<Notice>, runtime: Runtime) -> Result<(), SessionError> {
    let (sender, receiver) = ui_channel()?;
    for diagnostic in session.profile.settings().diagnostics() {
        sender.send(UiEvent::Notice {
            notice: Notice::new(NoticeTone::Warning, "", diagnostic.to_string()),
        });
    }
    if let Some(notice) = update {
        sender.send(UiEvent::Notice { notice });
    }
    let options = ShellOptions {
        version: ofx_upgrade::VERSION.to_owned(),
        model: session.setup.model().to_owned(),
        permission_mode: session.permission_mode,
        workspace_label: session
            .profile
            .workspace_root()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        workspace_root: session.profile.workspace_root().to_owned(),
        commands: slash_command_specs(),
    };
    let (commands, worker_commands) = tokio::sync::mpsc::unbounded_channel();
    let notices = sender.clone();
    let panics = PanicCapture::install(WORKER_THREAD, move |notice| {
        notices.send(UiEvent::Notice { notice });
    });
    let worker = spawn_worker(
        session.setup,
        session.executions,
        sender,
        worker_commands,
        runtime,
    )?;
    let result = panics
        .contain_shell(|| {
            run_shell(options, receiver, move |command| {
                let _ = commands.send(command);
            })
        })
        .unwrap_or_else(|payload| panic::resume_unwind(payload));
    worker.finish(WORKER_SHUTDOWN_GRACE, &panics)?;
    Ok(result?)
}

fn spawn_worker(
    setup: AgentSetup,
    executions: ManagedExecutions,
    sender: UiEventSender,
    commands: tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    runtime: Runtime,
) -> io::Result<Worker> {
    let emit = Arc::new(move |event: UiEvent| {
        sender.send(event);
    });
    let refreshes = setup.refreshes();
    let controller = Controller::new(setup, emit);
    Worker::spawn(move || {
        runtime.block_on(async {
            controller.run(commands).await;
            executions.shutdown().await;
            if let Some(refreshes) = refreshes {
                refreshes.settle().await;
            }
        });
        runtime.shutdown_timeout(Duration::from_millis(100));
    })
}

struct Worker {
    finished: mpsc::Receiver<()>,
}

impl Worker {
    fn spawn(work: impl FnOnce() + Send + 'static) -> io::Result<Self> {
        let (done, finished) = mpsc::channel();
        thread::Builder::new()
            .name(WORKER_THREAD.to_owned())
            .spawn(move || {
                work();
                let _ = done.send(());
            })?;
        Ok(Self { finished })
    }

    fn finish(self, grace: Duration, panics: &PanicCapture) -> Result<(), SessionError> {
        match self.finished.recv_timeout(grace) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => Ok(()),
            Err(RecvTimeoutError::Disconnected) => {
                Err(SessionError::AgentStopped(panics.take_worker_report()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::PoisonError;

    use super::*;
    use crate::app_panic_runtime::HOOK_TESTS;

    #[test]
    fn terminal_refusals_read_as_upstream_prints_them() {
        let message = |error| SessionError::Terminal(error).to_string();
        assert_eq!(
            message(TerminalError::TerminalTooSmall),
            "oh-fx needs at least 5 terminal rows."
        );
        assert_eq!(
            message(TerminalError::NotATerminal),
            "oh-fx requires an interactive terminal (TTY)."
        );
        assert_eq!(
            message(TerminalError::UnableToReadTerminalSize),
            "oh-fx: unable to read the terminal size"
        );
    }

    #[test]
    fn worker_panics_are_reported_instead_of_printed_and_clean_exits_are_quiet() {
        let _serial = HOOK_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
        let panics = PanicCapture::install(WORKER_THREAD, drop);
        let finished = Worker::spawn(|| {}).unwrap();
        assert!(finished.finish(Duration::from_secs(10), &panics).is_ok());
        let crashed = Worker::spawn(|| panic!("worker exploded")).unwrap();
        let message = crashed
            .finish(Duration::from_secs(10), &panics)
            .unwrap_err()
            .to_string();
        assert!(
            message.starts_with("oh-fx: the agent stopped unexpectedly: panicked at "),
            "{message}"
        );
        assert!(message.ends_with(": worker exploded"), "{message}");
        assert_eq!(panics.take_worker_report(), None);
    }
}
