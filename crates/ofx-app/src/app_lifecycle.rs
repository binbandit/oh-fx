use std::env;
use std::fmt;
use std::io::{self, IsTerminal};
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use ofx_cli::{LaunchModifiers, RequestedResume};
use ofx_contract::{Notice, NoticeTone, PermissionMode, UiCommand, UiEvent};
use ofx_exec::{ManagedExecutions, SessionSupervisor};
use ofx_tui::{
    Opening, ShellOptions, TerminalError, UiEventReceiver, UiEventSender, run_shell, ui_channel,
};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use crate::app_agent_runtime::Controller;
use crate::app_bootstrap_runtime::{AgentSetup, Launch, Profile, ProfileError};
use crate::app_commands::{slash_command_categories, slash_command_specs};
use crate::app_panic_runtime::PanicCapture;
use crate::app_session_runtime::{
    LaunchOverrides, Persistence, configured_preferences, open_store, running_provider,
};
use crate::app_upgrade_runtime;
use crate::codex_provider::{DetachedRefreshes, SubscriptionEndpoints};
use crate::file_mention_runtime::WorkspaceFileMentions;
use crate::native::NativeClipboard;
use crate::prompt_history_runtime::PromptHistoryRuntime;
use startup_resume::open_requested;

mod startup_resume;

const WORKER_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const WORKER_THREAD: &str = "oh-fx-agent";

const SELF_EXE_NOT_FOUND: &str = "SelfExeNotFound";

struct Session {
    profile: Profile,
    setup: AgentSetup,
    executions: ManagedExecutions,
    permission_mode: PermissionMode,
    persistence: Option<Persistence>,
    opening: Opening,
}

pub fn run_interactive(modifiers: &LaunchModifiers, resume: Option<&RequestedResume>) -> ExitCode {
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
    let session = match runtime.block_on(bootstrap(modifiers, resume)) {
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

async fn bootstrap(
    modifiers: &LaunchModifiers,
    resume: Option<&RequestedResume>,
) -> Result<Session, Vec<String>> {
    let mut profile = Profile::load().map_err(|error| profile_failure_lines(&error))?;
    profile
        .apply_launch(
            modifiers.additional_directories(),
            modifiers.saved_directories_suppressed(),
        )
        .map_err(|error| vec![failure_line(&error)])?;
    let supervisor = SessionSupervisor::current_executable()
        .map_err(|_| vec![failure_line(&SELF_EXE_NOT_FOUND)])?;
    let executions = ManagedExecutions::new(supervisor);
    let store = open_store(&profile);
    let resumed = match resume {
        Some(requested) => {
            open_requested(store.as_ref(), &mut profile, requested).map_err(|line| vec![line])?
        }
        None => None,
    };
    let saved = resumed
        .as_ref()
        .map(|resumed| resumed.session.preferences());
    let settings = profile.settings();
    let permission_mode = settings.permission_mode(&|name| env::var(name).ok());
    let setup = profile
        .connect_interactive(
            Launch {
                model: modifiers.model(),
                permission_mode,
                system_prompt: None,
                reasoning_effort: modifiers
                    .reasoning_effort()
                    .cloned()
                    .or_else(|| saved.map(|preferences| preferences.effort.clone()))
                    .unwrap_or_else(|| settings.reasoning_effort())
                    .into_named(),
                fast_mode: modifiers
                    .fast_mode()
                    .or_else(|| saved.map(|preferences| preferences.fast_mode)),
                context_limits: modifiers.context_limit_overrides(),
                command_timeout: None,
                executions: &executions,
                endpoints: SubscriptionEndpoints::default(),
                web_fetch_progress: None,
            },
            &CancellationToken::new(),
        )
        .await
        .map_err(|error| vec![failure_line(&error)])?;
    let opening = match &resumed {
        Some(resumed) => Opening::Transcript(
            resumed
                .session
                .transcript()
                .map_err(|error| vec![failure_line(&error)])?,
        ),
        None if resume == Some(&RequestedResume::Pick) => Opening::SessionPicker,
        None => Opening::Welcome,
    };
    let persistence = match (store, running_provider(&setup)) {
        (Ok(store), Ok(provider)) => {
            let preferences = configured_preferences(&profile, &setup, provider.clone());
            let overrides = LaunchOverrides {
                model: modifiers.model().map(|_| setup.model().to_owned()),
                effort: modifiers.reasoning_effort().cloned(),
                fast_mode: modifiers.fast_mode(),
            };
            Some(Persistence::new(
                store,
                provider,
                preferences,
                overrides,
                resumed,
            ))
        }
        (_, Err(error)) if resumed.is_some() => return Err(vec![failure_line(&error)]),
        _ => None,
    };
    Ok(Session {
        profile,
        setup,
        executions,
        permission_mode,
        persistence,
        opening,
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
    let (prompt_history, history_notice) = PromptHistoryRuntime::initialize(
        session.profile.data_dir(),
        session.profile.workspace_root(),
    )
    .into_shell_history(session.profile.settings().prompt_history_enabled());
    if let Some(notice) = history_notice {
        sender.send(UiEvent::Notice { notice });
    }
    if let Some(notice) = update {
        sender.send(UiEvent::Notice { notice });
    }
    let options = ShellOptions {
        version: ofx_upgrade::VERSION.to_owned(),
        model: session.setup.model().to_owned(),
        permission_mode: session.permission_mode,
        full_access_warning: session.permission_mode == PermissionMode::Yolo
            && !session.profile.settings().yolo_acknowledged(),
        workspace_label: session
            .profile
            .workspace_root()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        workspace_root: session.profile.workspace_root().to_owned(),
        commands: slash_command_specs(),
        command_categories: slash_command_categories(),
        prompt_history,
        file_mentions: Some(Box::new(WorkspaceFileMentions::start(
            session.profile.workspace_root(),
            session.profile.cache_dir(),
        ))),
        opening: session.opening,
    };
    let picking = matches!(options.opening, Opening::SessionPicker);
    let refreshes = session.setup.refreshes();
    let agent = agent_work(
        session.setup,
        (session.persistence, picking),
        session.executions,
        runtime,
    );
    host(options, sender, receiver, refreshes.as_deref(), agent)
}

fn agent_work(
    setup: AgentSetup,
    (persistence, pick_at_start): (Option<Persistence>, bool),
    executions: ManagedExecutions,
    runtime: Runtime,
) -> impl FnOnce(UiEventSender, UnboundedReceiver<UiCommand>) + Send + 'static {
    let refreshes = setup.refreshes();
    move |events, commands| {
        let controller = Controller::new(
            setup,
            Arc::new(move |event| events.send(event)),
            persistence,
            pick_at_start,
        );
        runtime.block_on(async {
            controller.run(commands).await;
            executions.shutdown().await;
            if let Some(refreshes) = refreshes {
                refreshes.settle().await;
            }
        });
        runtime.shutdown_timeout(Duration::from_millis(100));
    }
}

fn host(
    options: ShellOptions,
    events: UiEventSender,
    receiver: UiEventReceiver,
    refreshes: Option<&DetachedRefreshes>,
    work: impl FnOnce(UiEventSender, UnboundedReceiver<UiCommand>) + Send + 'static,
) -> Result<(), SessionError> {
    let (commands, worker_commands) = tokio::sync::mpsc::unbounded_channel();
    let notices = events.clone();
    let panics = PanicCapture::install(WORKER_THREAD, move |notice| {
        notices.send(UiEvent::Notice { notice });
    });
    let worker = Worker::spawn(events.clone(), move || work(events, worker_commands))?;
    let result = panics
        .contain_shell(|| {
            run_shell(options, receiver, NativeClipboard, move |command| {
                let _ = commands.send(command);
            })
        })
        .unwrap_or_else(|payload| panic::resume_unwind(payload));
    worker.finish(refreshes, &panics)?;
    Ok(result?)
}

struct Worker {
    finished: mpsc::Receiver<()>,
}

impl Worker {
    fn spawn(shell: UiEventSender, work: impl FnOnce() + Send + 'static) -> io::Result<Self> {
        let (done, finished) = mpsc::channel();
        thread::Builder::new()
            .name(WORKER_THREAD.to_owned())
            .spawn(move || {
                if panic::catch_unwind(AssertUnwindSafe(work)).is_ok() {
                    let _ = done.send(());
                } else {
                    drop(done);
                    shell.send(UiEvent::ExitRequested);
                }
            })?;
        Ok(Self { finished })
    }

    fn finish(
        self,
        refreshes: Option<&DetachedRefreshes>,
        panics: &PanicCapture,
    ) -> Result<(), SessionError> {
        if let Some(refreshes) = refreshes {
            refreshes.close();
        }
        loop {
            match self.finished.recv_timeout(WORKER_SHUTDOWN_GRACE) {
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(SessionError::AgentStopped(panics.take_worker_report()));
                }
                Err(RecvTimeoutError::Timeout)
                    if refreshes.is_some_and(DetachedRefreshes::wait_for_running) => {}
                Ok(()) | Err(RecvTimeoutError::Timeout) => return Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests;
