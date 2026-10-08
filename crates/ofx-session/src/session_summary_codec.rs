use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub workspace_root: String,
    pub origin_workspace_root: String,
    pub title: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub conversation_language: String,
    pub history_len: usize,
    pub has_checkpoint: bool,
}

impl SessionSummary {
    pub fn has_resumable_content(&self) -> bool {
        self.history_len != 0 || self.has_checkpoint
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeContinuation {
    pub updated_at_ms: i64,
    pub id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResumablePage {
    pub summaries: Vec<SessionSummary>,
    pub has_more: bool,
}

pub(crate) fn sort_summaries_newest_first(summaries: &mut [SessionSummary]) {
    summaries.sort_unstable_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then_with(|| b.id.cmp(&a.id))
    });
}

pub(crate) fn listed_page_from_summaries(
    summaries: &[SessionSummary],
    workspace_root: Option<&str>,
    continuation: Option<&ResumeContinuation>,
    limit: usize,
) -> ResumablePage {
    let mut page = ResumablePage::default();
    for summary in summaries {
        if workspace_root.is_some_and(|root| summary.workspace_root != root)
            || continuation.is_some_and(|position| !summary_follows(summary, position))
        {
            continue;
        }
        if page.summaries.len() >= limit {
            page.has_more = true;
            break;
        }
        page.summaries.push(summary.clone());
    }
    page
}

pub(crate) fn resumable_page_from_summaries(
    summaries: &[SessionSummary],
    workspace_root: Option<&str>,
    active_id: Option<&str>,
    continuation: Option<&ResumeContinuation>,
    limit: usize,
) -> ResumablePage {
    let limit = limit.max(1);
    let mut page = ResumablePage::default();
    for summary in summaries {
        if workspace_root.is_some_and(|root| summary.workspace_root != root)
            || !summary.has_resumable_content()
            || active_id == Some(summary.id.as_str())
            || continuation.is_some_and(|position| !summary_follows(summary, position))
        {
            continue;
        }
        if page.summaries.len() >= limit {
            page.has_more = true;
            break;
        }
        page.summaries.push(summary.clone());
    }
    page
}

fn summary_follows(summary: &SessionSummary, continuation: &ResumeContinuation) -> bool {
    match summary.updated_at_ms.cmp(&continuation.updated_at_ms) {
        Ordering::Equal => summary.id < continuation.id,
        ordering => ordering == Ordering::Less,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str, updated_at_ms: i64, history_len: usize) -> SessionSummary {
        SessionSummary {
            id: id.to_owned(),
            workspace_root: "/workspace".to_owned(),
            origin_workspace_root: "/workspace".to_owned(),
            title: None,
            created_at_ms: 1,
            updated_at_ms,
            conversation_language: "en".to_owned(),
            history_len,
            has_checkpoint: false,
        }
    }

    #[test]
    fn checkpoint_only_summaries_are_resumable_without_changing_turn_counts() {
        let mut checkpoint = summary("checkpoint", 2, 0);
        checkpoint.has_checkpoint = true;
        let empty = summary("empty", 2, 0);
        let page = resumable_page_from_summaries(&[checkpoint, empty], None, None, None, 10);
        assert_eq!(page.summaries.len(), 1);
        assert_eq!(page.summaries[0].id, "checkpoint");
        assert!(page.summaries[0].has_checkpoint);
        assert_eq!(page.summaries[0].history_len, 0);
    }

    #[test]
    fn summary_pages_filter_and_preserve_append_cursor_order() {
        let mut summaries = vec![
            summary("old", 1, 1),
            summary("new", 3, 1),
            summary("active", 2, 1),
        ];
        sort_summaries_newest_first(&mut summaries);
        let page =
            resumable_page_from_summaries(&summaries, Some("/workspace"), Some("active"), None, 1);
        assert_eq!(page.summaries.len(), 1);
        assert_eq!(page.summaries[0].id, "new");
        assert!(page.has_more);
    }

    #[test]
    fn continuations_resume_after_the_last_row_with_ties_broken_by_id() {
        let mut summaries = vec![
            summary("a", 5, 1),
            summary("c", 5, 1),
            summary("b", 5, 1),
            summary("z", 4, 1),
        ];
        sort_summaries_newest_first(&mut summaries);
        let ids: Vec<_> = summaries.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["c", "b", "a", "z"]);
        let first = resumable_page_from_summaries(&summaries, None, None, None, 2);
        assert!(first.has_more);
        let last = first.summaries.last().unwrap();
        let after = ResumeContinuation {
            updated_at_ms: last.updated_at_ms,
            id: last.id.clone(),
        };
        let second = resumable_page_from_summaries(&summaries, None, None, Some(&after), 2);
        let ids: Vec<_> = second.summaries.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["a", "z"]);
        assert!(!second.has_more);
    }

    #[test]
    fn pages_hold_at_least_one_row_and_filter_other_workspaces() {
        let mut other = summary("other", 9, 1);
        other.workspace_root = "/elsewhere".to_owned();
        let summaries = vec![other, summary("here", 1, 1)];
        let page = resumable_page_from_summaries(&summaries, Some("/workspace"), None, None, 0);
        assert_eq!(page.summaries.len(), 1);
        assert_eq!(page.summaries[0].id, "here");
        assert!(!page.has_more);
    }
}
