use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_cli::{NO_ULTRAFAST_ARG, ULTRAFAST_ARG, UPGRADE_RELAUNCH_ARG};

#[derive(Debug)]
struct Plan {
    executable: PathBuf,
    session_id: Option<String>,
    ultrafast: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Relaunch {
    plan: Arc<Mutex<Option<Plan>>>,
    launch: Arc<[OsString]>,
}

impl Relaunch {
    pub(crate) fn carrying(launch: Vec<OsString>) -> Self {
        Self {
            plan: Arc::default(),
            launch: launch.into(),
        }
    }

    pub(crate) fn request(&self, executable: PathBuf) {
        *self.plan() = Some(Plan {
            executable,
            session_id: None,
            ultrafast: None,
        });
    }

    pub(crate) fn hand_off(&self, session_id: &str, ultrafast: Option<bool>) {
        if let Some(plan) = self.plan().as_mut() {
            plan.session_id = Some(session_id.to_owned());
            plan.ultrafast = ultrafast;
        }
    }

    pub(super) fn run(&self) -> Result<(), RelaunchFailure> {
        self.run_with(CommandExt::exec)
    }

    pub(crate) fn run_with(
        &self,
        exec: impl FnOnce(&mut Command) -> io::Error,
    ) -> Result<(), RelaunchFailure> {
        let Some(plan) = self.plan().take() else {
            return Ok(());
        };
        let Some(session_id) = plan.session_id else {
            return Err(RelaunchFailure::NoHandoff);
        };
        let mut command = Command::new(plan.executable);
        command.args(self.launch.iter());
        if let Some(enabled) = plan.ultrafast {
            command.arg(if enabled {
                ULTRAFAST_ARG
            } else {
                NO_ULTRAFAST_ARG
            });
        }
        command.args(["resume", &session_id, UPGRADE_RELAUNCH_ARG]);
        let error = exec(&mut command);
        Err(RelaunchFailure::Exec { error, session_id })
    }

    fn plan(&self) -> MutexGuard<'_, Option<Plan>> {
        self.plan.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug)]
pub(crate) enum RelaunchFailure {
    Exec {
        error: io::Error,
        session_id: String,
    },
    NoHandoff,
}

impl fmt::Display for RelaunchFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exec { error, session_id } => write!(
                formatter,
                "oh-fx: upgrade installed, but relaunch failed: {error}\nContinue session with: oh-fx --resume {session_id}"
            ),
            Self::NoHandoff => formatter.write_str(
                "oh-fx: upgrade installed, but no validated resume handoff was available. Your conversation remains on disk; run `oh-fx doctor`.",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    fn exec_into(argv: &mut Vec<String>) -> impl FnOnce(&mut Command) -> io::Error {
        move |command| {
            argv.push(command.get_program().to_string_lossy().into_owned());
            argv.extend(
                command
                    .get_args()
                    .map(OsStr::to_string_lossy)
                    .map(String::from),
            );
            io::Error::from(io::ErrorKind::NotFound)
        }
    }

    #[test]
    fn a_relaunch_resumes_the_handed_off_session_and_explains_a_failed_exec() {
        let relaunch = Relaunch::default();
        relaunch.request(PathBuf::from("/tmp/oh-fx-upgraded"));
        relaunch.hand_off("session-123", None);
        let mut argv = Vec::new();
        let failure = relaunch.run_with(exec_into(&mut argv)).unwrap_err();
        assert_eq!(
            argv,
            [
                "/tmp/oh-fx-upgraded",
                "resume",
                "session-123",
                "--upgrade-relaunch"
            ]
        );
        assert_eq!(
            failure.to_string(),
            format!(
                "oh-fx: upgrade installed, but relaunch failed: {}\nContinue session with: oh-fx --resume session-123",
                io::Error::from(io::ErrorKind::NotFound)
            )
        );
    }

    #[test]
    fn a_relaunch_carries_the_launch_flags_ahead_of_resume() {
        let relaunch = Relaunch::carrying(
            [
                "--add-dir",
                "/tmp/cli-only",
                "--no-additional-dirs",
                "--context-limit",
                "skill_chunk_bytes=4096",
            ]
            .map(OsString::from)
            .to_vec(),
        );
        relaunch.request(PathBuf::from("/tmp/oh-fx-upgraded"));
        relaunch.hand_off("session-123", None);
        let mut argv = Vec::new();
        let _ = relaunch.run_with(exec_into(&mut argv));
        assert_eq!(
            argv,
            [
                "/tmp/oh-fx-upgraded",
                "--add-dir",
                "/tmp/cli-only",
                "--no-additional-dirs",
                "--context-limit",
                "skill_chunk_bytes=4096",
                "resume",
                "session-123",
                "--upgrade-relaunch"
            ]
        );
    }

    #[test]
    fn a_relaunch_keeps_the_ultra_choice_the_shell_made() {
        for (choice, flag) in [(Some(true), "--ultrafast"), (Some(false), "--no-ultrafast")] {
            let relaunch =
                Relaunch::carrying(["--add-dir", "/tmp/cli-only"].map(OsString::from).to_vec());
            relaunch.request(PathBuf::from("/tmp/oh-fx-upgraded"));
            relaunch.hand_off("session-123", choice);
            let mut argv = Vec::new();
            let _ = relaunch.run_with(exec_into(&mut argv));
            assert_eq!(
                argv,
                [
                    "/tmp/oh-fx-upgraded",
                    "--add-dir",
                    "/tmp/cli-only",
                    flag,
                    "resume",
                    "session-123",
                    "--upgrade-relaunch"
                ]
            );
        }
    }

    #[test]
    fn a_relaunch_never_runs_without_a_validated_handoff() {
        let relaunch = Relaunch::default();
        relaunch.request(PathBuf::from("/tmp/oh-fx-upgraded"));
        let mut argv = Vec::new();
        let failure = relaunch.run_with(exec_into(&mut argv)).unwrap_err();
        assert!(argv.is_empty());
        assert_eq!(
            failure.to_string(),
            "oh-fx: upgrade installed, but no validated resume handoff was available. Your conversation remains on disk; run `oh-fx doctor`."
        );
    }

    #[test]
    fn a_handoff_alone_relaunches_nothing() {
        let relaunch = Relaunch::default();
        relaunch.hand_off("session-123", None);
        let mut argv = Vec::new();
        assert!(relaunch.run_with(exec_into(&mut argv)).is_ok());
        assert!(argv.is_empty());
    }
}
