use ofx_config::PrivateDir;

use crate::json_fields::{Json, parse_json};
use crate::session_event::{ConversationEvent, UserEvent};
use crate::session_log::managed_file::read_managed_file;
use crate::session_log::{SavedHistory, SavedTurn};

const MAX_TITLE_WORDS: usize = 8;
pub const MAX_TITLE_BYTES: usize = 240;
const FALLBACK_TITLE: &str = "Untitled session";
const PROMPT_TRIM: &[char] = &[' ', '\t', '\r', '\n'];
const LINE_TRIM: &[char] = &[' ', '\t', '\r'];
const SIDECAR_FILE: &str = "display.json";
const MAX_SIDECAR_BYTES: usize = 16 * 1024;
const SIDECAR_SCHEMA_VERSION: u64 = 1;

pub(crate) fn derive_display_title(history: &SavedHistory) -> String {
    history
        .turns
        .iter()
        .find_map(|turn| match turn.events.first() {
            Some(ConversationEvent::User(user)) => titled_prompt(&user.text),
            _ => None,
        })
        .and_then(first_line_title)
        .unwrap_or_else(|| FALLBACK_TITLE.to_owned())
}

pub(crate) fn prompt_title(prompt: &str) -> Option<String> {
    titled_prompt(prompt).and_then(first_line_title)
}

pub(crate) fn history_title<'a>(
    turns: usize,
    mut prompts: impl Iterator<Item = &'a str>,
) -> Option<String> {
    if turns == 0 {
        return None;
    }
    let title = prompts
        .find_map(titled_prompt)
        .and_then(first_line_title)
        .unwrap_or_else(|| FALLBACK_TITLE.to_owned());
    Some(title)
}

pub(crate) fn read_sidecar_title(dir: &PrivateDir) -> Option<String> {
    let bytes = read_managed_file(dir, SIDECAR_FILE, MAX_SIDECAR_BYTES).ok()??;
    let sidecar = parse_json(&bytes).ok()?;
    let optional_text =
        |key| matches!(sidecar.get(key)?, Json::Null | Json::String(_)).then_some(());
    (sidecar.get("schema_version")?.as_u64()? == SIDECAR_SCHEMA_VERSION).then_some(())?;
    optional_text("preview")?;
    optional_text("origin_workspace_root")?;
    let title = sidecar.get("title")?.as_str()?;
    (!title.is_empty()).then(|| title.to_owned())
}

fn first_line_title(prompt: &str) -> Option<String> {
    prompt
        .split('\n')
        .map(|line| line.trim_matches(LINE_TRIM))
        .find(|line| !line.is_empty())
        .map(capped_title)
        .filter(|title| title != FALLBACK_TITLE)
}

fn titled_prompt(text: &str) -> Option<&str> {
    let trimmed = text.trim_matches(PROMPT_TRIM);
    let slash_command = trimmed.starts_with('/') && !trimmed.contains('\n');
    (!trimmed.is_empty() && !slash_command).then_some(text)
}

fn capped_title(line: &str) -> String {
    let mut title = String::new();
    for (index, word) in line
        .split(|character: char| matches!(character, ' ' | '\t'..='\r'))
        .filter(|word| !word.is_empty())
        .take(MAX_TITLE_WORDS)
        .enumerate()
    {
        let separator = usize::from(index > 0);
        let remaining = MAX_TITLE_BYTES.saturating_sub(title.len() + separator);
        let take = word.floor_char_boundary(remaining);
        if take == 0 {
            break;
        }
        if index > 0 {
            title.push(' ');
        }
        title.push_str(&word[..take]);
    }
    if title.is_empty() {
        return FALLBACK_TITLE.to_owned();
    }
    title
}

pub fn prompt_display_title(prompt: &str) -> String {
    derive_display_title(&SavedHistory {
        compacted: None,
        turns: vec![SavedTurn {
            events: vec![ConversationEvent::User(UserEvent::new(prompt))],
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_event::UserEvent;
    use crate::session_log::{CompactedHistory, SavedTurn};

    fn history(prompts: &[&str]) -> SavedHistory {
        SavedHistory {
            compacted: None,
            turns: prompts
                .iter()
                .map(|prompt| SavedTurn {
                    events: vec![ConversationEvent::User(UserEvent::new(
                        (*prompt).to_owned(),
                    ))],
                })
                .collect(),
        }
    }

    fn title(prompts: &[&str]) -> String {
        derive_display_title(&history(prompts))
    }

    #[test]
    fn the_title_is_the_first_eight_words_of_the_first_real_prompt() {
        assert_eq!(
            title(&[
                "/model gpt",
                "  \n",
                "\n  fix the  flaky\ttest in ci please now today ok\nmore"
            ]),
            "fix the flaky test in ci please now"
        );
        assert_eq!(title(&["/compact\nnotes"]), "/compact");
        assert_eq!(title(&["one\x0btwo"]), "one two");
    }

    #[test]
    fn sessions_without_a_usable_prompt_fall_back_to_the_untitled_label() {
        assert_eq!(title(&[]), FALLBACK_TITLE);
        assert_eq!(title(&["/help", " \n\t"]), FALLBACK_TITLE);
        let mut compacted = history(&[]);
        compacted.compacted = Some(CompactedHistory {
            summary: "earlier".to_owned(),
            removed_turn_count: 1,
            compaction_count: 1,
        });
        assert_eq!(derive_display_title(&compacted), FALLBACK_TITLE);
    }

    #[test]
    fn a_prompt_names_a_session_only_when_it_is_not_a_command_or_blank() {
        assert_eq!(
            prompt_title("  \n  fix the flaky test\nmore").as_deref(),
            Some("fix the flaky test")
        );
        assert_eq!(prompt_title("/help"), None);
        assert_eq!(prompt_title(" \n\t"), None);
        assert_eq!(prompt_title("\x0b\x0c"), None);
        assert_eq!(prompt_title(FALLBACK_TITLE), None);
    }

    #[test]
    fn long_titles_stop_at_240_bytes_on_a_character_boundary() {
        let word = "é".repeat(100);
        let capped = title(&[&format!("{word} {word} next")]);
        assert_eq!(capped.len(), 239);
        assert!(capped.ends_with(&format!(" {}", "é".repeat(19))));
        let ascii = "a".repeat(300);
        assert_eq!(title(&[&ascii]).len(), 240);
    }

    #[test]
    fn a_single_prompt_titles_a_session_as_its_history_would() {
        assert_eq!(
            prompt_display_title("  \n  fix the flaky test\nmore"),
            "fix the flaky test"
        );
        assert_eq!(prompt_display_title("/help"), FALLBACK_TITLE);
        assert_eq!(prompt_display_title("/model x"), FALLBACK_TITLE);
    }
}
