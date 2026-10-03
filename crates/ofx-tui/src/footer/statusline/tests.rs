use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

struct CountingIdentity {
    refreshes: Arc<AtomicUsize>,
}

impl WorkspaceIdentitySource for CountingIdentity {
    fn refresh(&mut self) -> WorkspaceIdentity {
        let count = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
        WorkspaceIdentity {
            label: "/work".to_owned(),
            branch: Some(format!("refresh-{count}")),
        }
    }
}

fn toggles(items: &[StatuslineItem]) -> StatuslineToggles {
    let mut toggles = StatuslineToggles::default();
    for item in items {
        toggles.set(*item, true);
    }
    toggles
}

fn counting(items: &[StatuslineItem]) -> (Statusline, Arc<AtomicUsize>) {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let source = CountingIdentity {
        refreshes: Arc::clone(&refreshes),
    };
    (
        Statusline::new(toggles(items), Some(Box::new(source))),
        refreshes,
    )
}

#[test]
fn context_usage_follows_the_latest_report_and_resets_with_the_conversation() {
    let (mut statusline, _) = counting(&[StatuslineItem::Context]);
    assert_eq!(statusline.view(), StatuslineView::default());
    statusline.usage_reported(Some(43_000), Some(1_000_000));
    assert_eq!(statusline.view().context_used, 43_000);
    assert_eq!(statusline.view().context_total, Some(1_000_000));
    statusline.usage_reported(None, Some(500_000));
    assert_eq!(statusline.view().context_used, 43_000);
    assert_eq!(statusline.view().context_total, Some(500_000));
    statusline.model_changed();
    assert_eq!(statusline.view().context_total, None);
    statusline.conversation_cleared();
    assert_eq!(statusline.view().context_used, 0);
}

#[test]
fn a_disabled_context_item_hides_usage_that_is_still_tracked() {
    let (mut statusline, _) = counting(&[]);
    statusline.usage_reported(Some(12_000), Some(750_000));
    assert_eq!(statusline.view(), StatuslineView::default());
    statusline.set(StatuslineItem::Context, true);
    assert_eq!(statusline.view().context_used, 12_000);
    assert_eq!(statusline.view().context_total, Some(750_000));
}

#[test]
fn the_workspace_identity_is_refreshed_only_while_its_item_is_on() {
    let (mut statusline, refreshes) = counting(&[]);
    statusline.refresh();
    assert_eq!(refreshes.load(Ordering::SeqCst), 0);
    assert_eq!(statusline.view().identity, None);
    statusline.set(StatuslineItem::Workspace, true);
    statusline.refresh();
    statusline.refresh();
    assert_eq!(refreshes.load(Ordering::SeqCst), 2);
    let identity = statusline.view().identity.unwrap();
    assert_eq!(identity.label, "/work");
    assert_eq!(identity.branch.as_deref(), Some("refresh-2"));
    statusline.set(StatuslineItem::Workspace, false);
    statusline.refresh();
    assert_eq!(refreshes.load(Ordering::SeqCst), 2);
    assert_eq!(statusline.view().identity, None);
    let unsourced = Statusline::new(toggles(&[StatuslineItem::Workspace]), None);
    assert_eq!(unsourced.view().identity, None);
}

#[test]
fn context_segments_use_whole_thousands_and_a_floored_percentage() {
    assert_eq!(context_segment(0, Some(1_000_000)), None);
    assert_eq!(context_segment(999, None).as_deref(), Some("0k"));
    assert_eq!(
        context_segment(43_000, Some(1_000_000)).as_deref(),
        Some("43k/1000k 4%")
    );
    assert_eq!(
        context_segment(199_999, Some(200_000)).as_deref(),
        Some("199k/200k 99%")
    );
    assert_eq!(
        context_segment(300_000, Some(200_000)).as_deref(),
        Some("300k/200k 150%")
    );
    assert_eq!(context_segment(5_000, Some(0)).as_deref(), Some("5k/0k 0%"));
}

#[test]
fn workspace_identities_respect_the_byte_budget() {
    let identity = WorkspaceIdentity {
        label: "/".repeat(600),
        branch: None,
    };
    let composed = workspace_identity_segment(&identity, 1_000, 512).unwrap();
    assert_eq!(composed.len(), 512);
    assert!(composed.starts_with(MARKER));
    assert_eq!(visible_width(&composed), 510);
    let tight = workspace_identity_segment(&identity, 1_000, 10).unwrap();
    assert_eq!(tight, format!("{MARKER}{}", "/".repeat(7)));
    let branch = WorkspaceIdentity {
        label: "/work".to_owned(),
        branch: Some(String::new()),
    };
    assert_eq!(
        workspace_identity_segment(&branch, 40, 512).as_deref(),
        Some("/work")
    );
    assert_eq!(workspace_identity_segment(&branch, 0, 512), None);
    assert_eq!(
        workspace_identity_segment(&WorkspaceIdentity::default(), 40, 512),
        None
    );
}

#[test]
fn identities_below_seven_cells_keep_only_the_path_tail() {
    let identity = WorkspaceIdentity {
        label: "/a/very/long/path".to_owned(),
        branch: Some("main".to_owned()),
    };
    assert_eq!(
        workspace_identity_segment(&identity, 6, 512).as_deref(),
        Some("…/path")
    );
    assert_eq!(
        workspace_identity_segment(&identity, 7, 512).as_deref(),
        Some("… (ma…)")
    );
    assert_eq!(
        workspace_identity_segment(&identity, 1, 512).as_deref(),
        Some(MARKER)
    );
}
