use ofx_contract::{HistoryEntry, Notice, NoticeTone};
use ofx_session::{ConversationEvent, InterruptReason, SavedTurn, SessionError, WritableSession};

const RESUMED_TOPIC: &str = "session resumed";
const SYSTEM_TOPIC: &str = "system";
const FAILED_TURN: &str = "failed";

pub(super) fn transcript(
    session: &WritableSession,
    title: &str,
) -> Result<Vec<HistoryEntry>, SessionError> {
    let mut entries = vec![HistoryEntry::Notice(Notice::new(
        NoticeTone::Neutral,
        RESUMED_TOPIC,
        title,
    ))];
    session.visit_transcript(|turn| replay_turn(turn, &mut entries))?;
    Ok(entries)
}

fn replay_turn(turn: SavedTurn, entries: &mut Vec<HistoryEntry>) {
    for event in turn.events {
        match event {
            ConversationEvent::User(user) => entries.push(HistoryEntry::User(user.text)),
            ConversationEvent::Steering(steering) if !steering.text.is_empty() => {
                entries.push(HistoryEntry::User(steering.text));
            }
            ConversationEvent::Assistant(assistant) if !assistant.text.is_empty() => {
                entries.push(HistoryEntry::Assistant(assistant.text));
            }
            ConversationEvent::Interrupted(interrupted) => {
                if let Some(partial) = interrupted.partial_text.filter(|text| !text.is_empty()) {
                    entries.push(HistoryEntry::Assistant(partial));
                }
                entries.push(match interrupted.reason {
                    InterruptReason::Cancelled => HistoryEntry::Cancelled,
                    InterruptReason::Failed => HistoryEntry::Notice(Notice::new(
                        NoticeTone::Error,
                        SYSTEM_TOPIC,
                        FAILED_TURN,
                    )),
                });
            }
            _ => {}
        }
    }
}
