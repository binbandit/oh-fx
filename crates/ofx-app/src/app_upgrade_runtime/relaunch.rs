use std::fmt;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_cli::UPGRADE_RELAUNCH_ARG;

#[derive(Debug)]
struct Plan {
    executable: PathBuf,
    session_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Relaunch(Arc<Mutex<Option<Plan>>>);

impl Relaunch {
    pub(super) fn request(&self, executable: PathBuf) {
        *self.plan() = Some(Plan {
            executable,
            session_id: None,
        });
    }

    pub(crate) fn hand_off(&self, session_id: &str) {
        if let Some(plan) = self.plan().as_mut() {
            plan.session_id = Some(session_id.to_owned());
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
        command.args(["resume", &session_id, UPGRADE_RELAUNCH_ARG]);
        let error = exec(&mut command);
        Err(RelaunchFailure::Exec { error, session_id })
    }

    fn plan(&self) -> MutexGuard<'_, Option<Plan>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
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
        relaunch.hand_off("session-123");
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
        relaunch.hand_off("session-123");
        let mut argv = Vec::new();
        assert!(relaunch.run_with(exec_into(&mut argv)).is_ok());
        assert!(argv.is_empty());
    }
}
