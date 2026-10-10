use std::path::Path;

use ofx_config::ProviderId;
use ofx_contract::{HistoryTurn, ReasoningEffort, TurnEnd};
use ofx_session::{SavedProvider, SessionPreferences};

use super::*;

fn preferences() -> SessionPreferences {
    SessionPreferences {
        provider: SavedProvider::new(ProviderId::Gateway, None).unwrap(),
        model: "openai/gpt-5".to_owned(),
        effort: ReasoningEffort::Auto,
        fast_mode: false,
    }
}

fn store(data: &Path, workspace: &str) -> SessionStore {
    SessionStore::open(data, workspace).unwrap()
}

fn saved(data: &Path, workspace: &str, prompt: &str) -> String {
    let mut session = store(data, workspace).start(preferences()).unwrap();
    session
        .record_turn(
            &HistoryTurn {
                user: prompt,
                steps: Vec::new(),
                steering: Vec::new(),
                files: &[],
                end: TurnEnd::Replied {
                    text: "done",
                    provider_replay: None,
                },
                images: &[],
            },
            &preferences().provider,
        )
        .unwrap();
    session.id().to_owned()
}

fn first_page(scope: SessionScope) -> PageRequest {
    PageRequest {
        scope,
        after: None,
        limit: 10,
    }
}

fn ids(page: &SessionPage) -> Vec<&str> {
    page.rows.iter().map(|row| row.id.as_str()).collect()
}

fn answered(answers: &[Answer]) -> Vec<(SessionScope, Vec<&str>)> {
    answers
        .iter()
        .map(|answer| {
            let page = answer.page.as_ref().unwrap();
            (page.scope, ids(page))
        })
        .collect()
}

async fn settle(
    listing: &mut SessionListing,
    store: &SessionStore,
    active_id: Option<&str>,
) -> Vec<Answer> {
    let scanned = listing.scanned().await;
    listing.finish(store, active_id, scanned, Instant::now())
}

fn scan_id(listing: &SessionListing) -> Option<tokio::task::Id> {
    listing.scan.as_ref().map(|scan| scan.task.id())
}

#[tokio::test]
async fn session_picker_loads_on_demand_and_shares_one_catalog_across_workspace_views() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("data");
    let here = saved(&data, "/here", "fix the renderer");
    let there = saved(&data, "/there", "review sessions");
    let store = store(&data, "/here");
    let mut listing = SessionListing::default();
    let now = Instant::now();
    assert!(matches!(
        listing.request(
            &store,
            None,
            first_page(SessionScope::CurrentWorkspace),
            now
        ),
        Ok(Listed::Waiting)
    ));
    let scan = scan_id(&listing);
    assert!(matches!(
        listing.request(&store, None, first_page(SessionScope::AllWorkspaces), now),
        Ok(Listed::Waiting)
    ));
    assert_eq!(scan_id(&listing), scan);
    let answers = settle(&mut listing, &store, None).await;
    assert_eq!(
        answered(&answers),
        [
            (SessionScope::CurrentWorkspace, vec![here.as_str()]),
            (
                SessionScope::AllWorkspaces,
                vec![there.as_str(), here.as_str()]
            ),
        ]
    );
    let later = saved(&data, "/here", "a later session");
    let Ok(Listed::Ready(page)) = listing.request(
        &store,
        None,
        first_page(SessionScope::AllWorkspaces),
        Instant::now(),
    ) else {
        panic!("the held catalog answers within its freshness window");
    };
    assert_eq!(ids(&page), [there.as_str(), here.as_str()]);
    let stale = Instant::now() + FRESH_FOR + Duration::from_millis(1);
    assert!(matches!(
        listing.request(&store, None, first_page(SessionScope::AllWorkspaces), stale),
        Ok(Listed::Waiting)
    ));
    let answers = settle(&mut listing, &store, None).await;
    assert_eq!(
        answered(&answers),
        [(
            SessionScope::AllWorkspaces,
            vec![later.as_str(), there.as_str(), here.as_str()]
        )]
    );
}

#[tokio::test]
async fn session_catalog_preload_feeds_the_picker_open_without_a_second_scan() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("data");
    let preloaded = saved(&data, "/here", "saved request");
    let store = store(&data, "/here");
    store.catalog().unwrap();
    let mut listing = SessionListing::default();
    listing.preload(&store, None, Instant::now());
    let scan = scan_id(&listing).expect("the preload scans");
    assert!(matches!(
        listing.request(
            &store,
            None,
            first_page(SessionScope::CurrentWorkspace),
            Instant::now()
        ),
        Ok(Listed::Waiting)
    ));
    assert_eq!(scan_id(&listing), Some(scan));
    let answers = settle(&mut listing, &store, None).await;
    assert_eq!(
        answered(&answers),
        [(SessionScope::CurrentWorkspace, vec![preloaded.as_str()])]
    );
    assert!(matches!(
        listing.request(
            &store,
            None,
            first_page(SessionScope::CurrentWorkspace),
            Instant::now()
        ),
        Ok(Listed::Ready(_))
    ));
    assert_eq!(scan_id(&listing), None);
}

#[tokio::test]
async fn session_catalog_preload_is_best_effort_and_never_duplicates_an_in_flight_scan() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("data");
    saved(&data, "/here", "saved request");
    let store = store(&data, "/here");
    let mut listing = SessionListing::default();
    listing.preload(&store, None, Instant::now());
    assert_eq!(scan_id(&listing), None);
    store.catalog().unwrap();
    listing.preload(&store, None, Instant::now());
    let first = scan_id(&listing).expect("the preload scans once an index exists");
    listing.preload(&store, None, Instant::now());
    assert_eq!(scan_id(&listing), Some(first));
    assert!(settle(&mut listing, &store, None).await.is_empty());
    listing.preload(&store, None, Instant::now());
    assert_eq!(scan_id(&listing), None);
}

#[tokio::test]
async fn session_picker_source_invalidation_discards_a_finished_catalog_and_pending_request() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("data");
    let first = saved(&data, "/here", "first");
    let second = saved(&data, "/here", "second");
    let store = store(&data, "/here");
    let mut listing = SessionListing::default();
    assert!(matches!(
        listing.request(
            &store,
            Some(&first),
            first_page(SessionScope::CurrentWorkspace),
            Instant::now()
        ),
        Ok(Listed::Waiting)
    ));
    let scanned = listing.scanned().await;
    assert!(
        listing
            .finish(&store, Some(&second), scanned, Instant::now())
            .is_empty()
    );
    assert!(scan_id(&listing).is_some());
    let answers = settle(&mut listing, &store, Some(&second)).await;
    assert_eq!(
        answered(&answers),
        [(SessionScope::CurrentWorkspace, vec![first.as_str()])]
    );
}

#[tokio::test]
async fn later_pages_come_from_the_held_catalog_after_its_freshness_window() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("data");
    let older = saved(&data, "/here", "older");
    let newer = saved(&data, "/here", "newer");
    let store = store(&data, "/here");
    let mut listing = SessionListing::default();
    let one = PageRequest {
        limit: 1,
        ..first_page(SessionScope::CurrentWorkspace)
    };
    listing
        .request(&store, None, one.clone(), Instant::now())
        .unwrap();
    let answers = settle(&mut listing, &store, None).await;
    let page = answers[0].page.as_ref().unwrap();
    assert_eq!(ids(page), [newer.as_str()]);
    assert!(page.has_more);
    saved(&data, "/here", "newest");
    let next = PageRequest {
        after: page.rows.last().map(|row| SessionCursor {
            updated_at_ms: row.updated_at_ms,
            id: row.id.clone(),
        }),
        ..one
    };
    let stale = Instant::now() + FRESH_FOR + Duration::from_millis(1);
    let Ok(Listed::Ready(page)) = listing.request(&store, None, next, stale) else {
        panic!("later pages continue the held catalog");
    };
    assert_eq!(ids(&page), [older.as_str()]);
    assert!(!page.has_more);
}
