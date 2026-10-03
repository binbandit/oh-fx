use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::io;

use super::super::session_upgrader::UpgradeState;
use super::*;

const UPGRADED: &str = "/tmp/oh-fx-upgraded";

#[derive(Default)]
struct Conversation {
    prepared: Cell<usize>,
    failure: Option<fn() -> Unresumable>,
    requested: Option<Relaunch>,
}

impl ResumeHandoff for Conversation {
    fn prepare_resume_handoff(&self) -> Result<(), Unresumable> {
        self.prepared.set(self.prepared.get() + 1);
        self.failure.map_or(Ok(()), |failure| Err(failure()))
    }

    fn request_resume_handoff(&mut self, relaunch: Relaunch) {
        relaunch.hand_off("session-123");
        self.requested = Some(relaunch);
    }
}

struct Applied {
    relaunched: bool,
    events: Vec<UiEvent>,
    resolved: usize,
    shortcut: UpgradeShortcut,
}

impl Applied {
    fn notices(&self) -> Vec<(NoticeTone, &str, &str)> {
        self.events
            .iter()
            .filter_map(|event| match event {
                UiEvent::Notice { notice } => {
                    Some((notice.tone, notice.topic.as_str(), notice.body.as_str()))
                }
                _ => None,
            })
            .collect()
    }

    fn exit_requested(&self) -> bool {
        self.events.contains(&UiEvent::ExitRequested)
    }
}

fn apply(
    state: Option<UpgradeState>,
    conversation: Option<&mut Conversation>,
    executable: Result<PathBuf, UpgradeError>,
) -> Applied {
    let shortcut = UpgradeShortcut::new(state.map(Readiness::settled), Relaunch::default());
    let events = RefCell::new(Vec::new());
    let resolved = Cell::new(0);
    let relaunched = shortcut.apply_with(
        conversation.map(|conversation| conversation as &mut dyn ResumeHandoff),
        &|event| events.borrow_mut().push(event),
        || {
            resolved.set(resolved.get() + 1);
            executable
        },
    );
    Applied {
        relaunched,
        events: events.into_inner(),
        resolved: resolved.get(),
        shortcut,
    }
}

fn relaunch_argv(shortcut: &UpgradeShortcut) -> Vec<String> {
    let mut argv = Vec::new();
    let _ = shortcut.relaunch.run_with(|command| {
        argv.push(command.get_program().to_string_lossy().into_owned());
        argv.extend(
            command
                .get_args()
                .map(OsStr::to_string_lossy)
                .map(String::from),
        );
        io::Error::from(io::ErrorKind::NotFound)
    });
    argv
}

#[test]
fn a_ready_upgrade_validates_the_conversation_then_requests_the_relaunch() {
    let mut conversation = Conversation::default();
    let applied = apply(
        Some(UpgradeState::Ready),
        Some(&mut conversation),
        Ok(PathBuf::from(UPGRADED)),
    );
    assert!(applied.relaunched);
    assert_eq!(applied.events, [UiEvent::ExitRequested]);
    assert_eq!(conversation.prepared.get(), 1);
    assert_eq!(applied.resolved, 1);
    assert!(conversation.requested.is_some());
    assert_eq!(
        relaunch_argv(&applied.shortcut),
        [UPGRADED, "resume", "session-123", "--upgrade-relaunch"]
    );
}

#[test]
fn an_unresolved_executable_path_keeps_the_session_running() {
    let mut conversation = Conversation::default();
    let applied = apply(
        Some(UpgradeState::Ready),
        Some(&mut conversation),
        Err(UpgradeError::SelfExeNotFound),
    );
    assert!(!applied.relaunched);
    assert!(!applied.exit_requested());
    assert_eq!(conversation.prepared.get(), 1);
    assert_eq!(applied.resolved, 1);
    assert!(conversation.requested.is_none());
    assert_eq!(
        applied.notices(),
        [(
            NoticeTone::Error,
            "upgrade",
            "upgrade installed, but the executable path could not be resolved: could not determine path of running binary; restart oh-fx manually"
        )]
    );
    assert!(relaunch_argv(&applied.shortcut).is_empty());
}

#[test]
fn an_unresumable_conversation_pauses_the_upgrade() {
    let mut conversation = Conversation {
        failure: Some(|| Unresumable::Session(SessionError::InvalidSessionFormat)),
        ..Conversation::default()
    };
    let applied = apply(
        Some(UpgradeState::Ready),
        Some(&mut conversation),
        Ok(PathBuf::from(UPGRADED)),
    );
    assert!(!applied.relaunched);
    assert!(!applied.exit_requested());
    assert_eq!(conversation.prepared.get(), 1);
    assert_eq!(applied.resolved, 0);
    assert!(conversation.requested.is_none());
    assert_eq!(
        applied.notices(),
        [(
            NoticeTone::Error,
            "upgrade",
            "upgrade paused because this conversation is not safely resumable: InvalidSessionFormat; run `oh-fx doctor` for recovery guidance"
        )]
    );
}

#[test]
fn a_reload_needs_an_active_session() {
    let applied = apply(Some(UpgradeState::Ready), None, Ok(PathBuf::from(UPGRADED)));
    assert!(!applied.relaunched);
    assert_eq!(applied.resolved, 0);
    assert_eq!(
        applied.notices(),
        [(
            NoticeTone::Error,
            "upgrade",
            "upgrade paused because this conversation is not safely resumable: SessionPersistenceUnavailable; run `oh-fx doctor` for recovery guidance"
        )]
    );
}

#[test]
fn an_upgrade_that_is_not_installed_yet_is_refused() {
    for state in [
        UpgradeState::Idle,
        UpgradeState::Checking,
        UpgradeState::Waiting,
        UpgradeState::Downloading,
        UpgradeState::Failed,
    ] {
        let mut conversation = Conversation::default();
        let applied = apply(
            Some(state),
            Some(&mut conversation),
            Ok(PathBuf::from(UPGRADED)),
        );
        assert!(!applied.relaunched);
        assert_eq!(conversation.prepared.get(), 0);
        assert_eq!(applied.resolved, 0);
        assert_eq!(
            applied.notices(),
            [(
                NoticeTone::Neutral,
                "upgrade",
                "no installed upgrade is ready"
            )]
        );
    }
}

#[test]
fn a_disabled_upgrader_says_so() {
    let mut conversation = Conversation::default();
    let applied = apply(None, Some(&mut conversation), Ok(PathBuf::from(UPGRADED)));
    assert!(!applied.relaunched);
    assert_eq!(conversation.prepared.get(), 0);
    assert_eq!(
        applied.notices(),
        [(NoticeTone::Neutral, "upgrade", "auto-upgrade is disabled")]
    );
}
