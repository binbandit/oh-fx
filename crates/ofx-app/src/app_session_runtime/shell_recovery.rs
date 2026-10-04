use ofx_contract::{HistoryEntry, Notice, NoticeTone};
use ofx_session::WritableSession;

pub(crate) const RECOVERY_TOPIC: &str = "recovery";
const CONTINUES_AUTOMATICALLY: &str = "model response recovery paused and continues automatically";
const CONTINUES_WITH_UNCERTAIN_TOOL: &str = "model response recovery paused and continues automatically; inspect the uncertain tool state if anything looks wrong";
const NOT_RESTARTED: &str = "oh-fx quit unexpectedly while this response was recovering, so it was not restarted. Send \"continue\" to retry it, or a new message to move on.";
pub(crate) const NOT_CONTINUED: &str = "the interrupted response could not continue automatically; it will try again on the next resume";

pub(super) struct ShellRecovery {
    pub(super) entries: Vec<HistoryEntry>,
    pub(super) continues: bool,
}

pub(super) fn shell_recovery(session: &mut WritableSession) -> Option<ShellRecovery> {
    let unclean = session.previous_owner_died() || session.recovery_was_asked();
    let shown = session.recovery_transcript()?;
    let mut entries = shown.entries;
    if unclean {
        session.mark_recovery_asked();
        entries.push(warning(NOT_RESTARTED));
        return Some(ShellRecovery {
            entries,
            continues: false,
        });
    }
    entries.push(warning(if shown.uncertain_tool {
        CONTINUES_WITH_UNCERTAIN_TOOL
    } else {
        CONTINUES_AUTOMATICALLY
    }));
    Some(ShellRecovery {
        entries,
        continues: true,
    })
}

fn warning(body: &str) -> HistoryEntry {
    HistoryEntry::Notice(Notice::new(NoticeTone::Warning, RECOVERY_TOPIC, body))
}
