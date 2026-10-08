use std::env;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, IsTerminal};
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use ofx_agent::WorkerRuntime;
use ofx_cli::{LaunchModifiers, RequestedResume};
use ofx_contract::{
    BoxFuture, DynamicTools, Notice, NoticeTone, PermissionMode, UiCommand, UiEvent,
};
use ofx_exec::{ManagedExecutions, SessionSupervisor};
use ofx_mcp::{McpRuntime, ShutdownMode, StartupPhase, render_workspace_diagnostic};
use ofx_text::encode_terminal_safe;
use ofx_tui::{
    Opening, ShellOptions, TerminalError, UiEventReceiver, UiEventSender, run_shell, ui_channel,
};
use ofx_workspace::StatuslineIdentity;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use crate::app_agent_runtime::Controller;
use crate::app_bootstrap_runtime::{
    AgentSetup, Launch, Login, Profile, ProfileError, provider_names,
};
use crate::app_commands::{slash_command_categories, slash_command_specs};
use crate::app_mcp_runtime;
use crate::app_panic_runtime::PanicCapture;
use crate::app_session_runtime::{
    LaunchOverrides, Persistence, configured_preferences, open_store, session_route,
};
use crate::app_steering_runtime::WaitingSteering;
use crate::app_upgrade_runtime::{
    self, InteractiveUpgrade, RelaunchFailure, SessionUpgrader, UpgradeShortcut,
};
use crate::codex_provider::{DetachedRefreshes, SubscriptionEndpoints};
use crate::file_mention_runtime::WorkspaceFileMentions;
use crate::herdr::{Herdr, HerdrObserver};
use crate::native::NativeClipboard;
use crate::prompt_history_runtime::PromptHistoryRuntime;
use crate::skill_mention_runtime::SkillMentions;
use crate::skills::Installations;
use startup_resume::open_requested;
pub use startup_status::{StartupStatus, StartupStatusError};

mod startup_resume;
pub(crate) mod startup_status;

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
    ultrafast_requested: bool,
    relaunch_args: Vec<OsString>,
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
    let update = app_upgrade_runtime::announce_update();
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
                mode: None,
            },
            &CancellationToken::new(),
        )
        .await
        .map_err(|error| vec![failure_line(&error)])?;
    let opening = match &resumed {
        Some(resumed) => Opening::Transcript(
            resumed
                .session
                .transcript(&setup)
                .map_err(|error| vec![failure_line(&error)])?,
        ),
        None if resume == Some(&RequestedResume::Pick) => Opening::SessionPicker,
        None => Opening::Welcome,
    };
    let persistence = match (store, session_route(&setup)) {
        (Ok(store), Ok(route)) => {
            let preferences = configured_preferences(&profile, &setup, route.provider.clone());
            let overrides = LaunchOverrides {
                model: modifiers.model().map(|_| setup.model().to_owned()),
                effort: modifiers.reasoning_effort().cloned(),
                fast_mode: modifiers.fast_mode(),
            };
            Some(Persistence::new(
                store,
                route,
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
        ultrafast_requested: modifiers.ultrafast_mode() == Some(true),
        relaunch_args: modifiers.relaunch_args().to_vec(),
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
    Relaunch(RelaunchFailure),
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
            Self::Relaunch(failure) => write!(formatter, "{failure}"),
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
    let steering = Arc::new(WorkerRuntime::default());
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
    let lifecycle = Herdr::from_env().map(Arc::new);
    let upgrade = InteractiveUpgrade::start(
        sender.clone(),
        session.relaunch_args,
        session.profile.settings().auto_upgrade_enabled(),
    );
    let options = ShellOptions {
        version: ofx_upgrade::VERSION.to_owned(),
        model: session.setup.model().to_owned(),
        provider: session.setup.provider().label().to_owned(),
        providers: provider_names(session.profile.settings()),
        permission_mode: session.permission_mode,
        full_access_warning: session.permission_mode == PermissionMode::Yolo
            && !session.profile.settings().yolo_acknowledged(),
        login_missing: session.setup.login() == Login::Missing,
        workspace_label: session
            .profile
            .workspace_root()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        startup_scrollback: session.profile.settings().startup_scrollback(),
        commands: slash_command_specs(),
        command_categories: slash_command_categories(),
        prompt_history,
        file_mentions: Some(Box::new(WorkspaceFileMentions::start(
            session.profile.workspace_root(),
            session.profile.cache_dir(),
        ))),
        skill_catalog: Some(Box::new(SkillMentions::new(session.setup.skills().clone()))),
        lifecycle: lifecycle.as_ref().map(|client| {
            Box::new(HerdrObserver(Arc::clone(client))) as Box<dyn ofx_tui::ForegroundLifecycle>
        }),
        steering: Some(Box::new(WaitingSteering(Arc::clone(&steering)))),
        opening: session.opening,
        statusline: session.setup.statusline(),
        workspace_identity: Some(Box::new(StatuslineIdentity::new(
            session.profile.workspace_root(),
        ))),
        theme: session.profile.settings().theme().map(str::to_owned),
    };
    let picking = matches!(options.opening, Opening::SessionPicker);
    let refreshes = session.setup.refreshes();
    let installations = session.setup.skills().installations();
    let agent = agent_work(
        session.setup,
        (session.persistence, picking, session.ultrafast_requested),
        session.executions,
        steering,
        runtime,
        lifecycle,
        upgrade.shortcut(),
    );
    host(
        options,
        sender,
        receiver,
        refreshes.as_deref(),
        Some(&installations),
        upgrade.upgrader(),
        agent,
    )?;
    upgrade.relaunch().map_err(SessionError::Relaunch)
}

fn agent_work(
    setup: AgentSetup,
    (persistence, pick_at_start, ultrafast_requested): (Option<Persistence>, bool, bool),
    executions: ManagedExecutions,
    steering: Arc<WorkerRuntime>,
    runtime: Runtime,
    herdr: Option<Arc<Herdr>>,
    upgrade: UpgradeShortcut,
) -> impl FnOnce(UiEventSender, UnboundedReceiver<UiCommand>) + Send + 'static {
    let refreshes = setup.refreshes();
    let mcp = setup.mcp().cloned();
    move |events, commands| {
        let notices = events.clone();
        let controller = Controller::new(
            setup,
            Arc::new(move |event| events.send(event)),
            persistence,
            pick_at_start,
            steering,
        )
        .with_herdr(herdr)
        .requesting_ultrafast(ultrafast_requested)
        .with_upgrade(upgrade);
        runtime.block_on(async {
            let discovery = mcp.clone().map(|mcp| {
                tokio::spawn::<BoxFuture<'static, ()>>(Box::pin(discover_mcp(mcp, notices)))
            });
            Box::pin(controller.run(commands)).await;
            if let Some(discovery) = discovery {
                discovery.abort();
            }
            if let Some(mcp) = &mcp {
                mcp.shutdown(ShutdownMode::Immediate).await;
            }
            executions.shutdown().await;
            if let Some(refreshes) = refreshes {
                refreshes.settle().await;
            }
        });
        runtime.shutdown_timeout(Duration::from_millis(100));
    }
}

async fn discover_mcp(mcp: Arc<McpRuntime>, events: UiEventSender) {
    let warn = |body: String| {
        events.send(UiEvent::Notice {
            notice: Notice::new(NoticeTone::Warning, "", body),
        });
    };
    for diagnostic in mcp.workspace_diagnostics() {
        warn(render_workspace_diagnostic(&diagnostic));
    }
    let pending = mcp.pending_workspace_names();
    if !pending.is_empty() {
        let names: Vec<String> = pending
            .iter()
            .map(|name| encode_terminal_safe(name.as_bytes(), usize::MAX).text)
            .collect();
        warn(format!(
            "Skipped unapproved project MCP servers: {}. Approve with /mcp trust approve <name>.",
            names.join(", ")
        ));
    }
    mcp.connect(StartupPhase::All).await;
    if let Some(body) = mcp.startup_notice() {
        events.send(UiEvent::Notice {
            notice: Notice::new(NoticeTone::Warning, app_mcp_runtime::TOPIC, body),
        });
    }
    let _ = mcp.tools();
    for notice in mcp.take_notices() {
        warn(notice);
    }
}

fn host(
    options: ShellOptions,
    events: UiEventSender,
    receiver: UiEventReceiver,
    refreshes: Option<&DetachedRefreshes>,
    installations: Option<&Installations>,
    upgrader: Option<&SessionUpgrader>,
    work: impl FnOnce(UiEventSender, UnboundedReceiver<UiCommand>) + Send + 'static,
) -> Result<(), SessionError> {
    let stop_upgrader = || {
        if let Some(upgrader) = upgrader {
            upgrader.stop_for_process_exit();
        }
    };
    let (commands, worker_commands) = tokio::sync::mpsc::unbounded_channel();
    let notices = events.clone();
    let panics = PanicCapture::install(WORKER_THREAD, move |notice| {
        notices.send(UiEvent::Notice { notice });
    });
    let worker = Worker::spawn(events.clone(), move || work(events, worker_commands))
        .inspect_err(|_| stop_upgrader())?;
    let result = panics
        .contain_shell(|| {
            run_shell(
                options,
                receiver,
                NativeClipboard,
                move |command| {
                    let _ = commands.send(command);
                },
                stop_upgrader,
            )
        })
        .unwrap_or_else(|payload| panic::resume_unwind(payload));
    stop_upgrader();
    worker.finish(refreshes, installations, &panics)?;
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
        installations: Option<&Installations>,
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
                    if installations.is_some_and(|installs| {
                        installs.wait_for_running(WORKER_SHUTDOWN_GRACE)
                    }) || refreshes.is_some_and(DetachedRefreshes::wait_for_running) => {}
                Ok(()) | Err(RecvTimeoutError::Timeout) => return Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests;
