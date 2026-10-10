use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::catalog_freshness::{SnapshotMetadata, failed_refresh, request_refresh};
use crate::error::McpError;
use crate::features::common::Listed;
use crate::features::prompts::Prompt;
use crate::features::resources::{Resource, ResourceTemplate};
use crate::protocol_messages::ServerCapabilities;
use crate::server_connection::{McpClient, lock};

#[derive(Debug, Clone)]
pub(crate) struct Snapshot<T> {
    pub(crate) items: Arc<[T]>,
    pub(crate) metadata: SnapshotMetadata,
}

#[derive(Default)]
pub(crate) struct FeatureCatalogs {
    pub(crate) refresh: tokio::sync::Mutex<()>,
    advertised: Mutex<ServerCapabilities>,
    resources: Mutex<Option<Snapshot<Resource>>>,
    templates: Mutex<Option<Snapshot<ResourceTemplate>>>,
    prompts: Mutex<Option<Snapshot<Prompt>>>,
}

impl FeatureCatalogs {
    pub(crate) fn reset(&self, capabilities: ServerCapabilities) {
        *lock(&self.advertised) = capabilities;
        *lock(&self.resources) = None;
        *lock(&self.templates) = None;
        *lock(&self.prompts) = None;
    }

    pub(crate) fn advertises_resources(&self) -> bool {
        lock(&self.advertised).resources.is_some()
    }

    pub(crate) fn advertises_prompts(&self) -> bool {
        lock(&self.advertised).prompts.is_some()
    }

    pub(crate) fn snapshot<T: FeatureCatalog>(&self) -> Option<Snapshot<T>> {
        lock(T::slot(self)).clone()
    }

    pub(crate) fn publish<T: FeatureCatalog>(&self, snapshot: Snapshot<T>) {
        *lock(T::slot(self)) = Some(snapshot);
    }

    pub(crate) fn record_failed_refresh<T: FeatureCatalog>(&self, items: &Arc<[T]>, now_ms: u64) {
        if let Some(snapshot) = lock(T::slot(self))
            .as_mut()
            .filter(|snapshot| Arc::ptr_eq(&snapshot.items, items))
        {
            snapshot.metadata = failed_refresh(snapshot.metadata, now_ms);
        }
    }

    pub(crate) fn request_refresh(&self) {
        expire(&self.resources);
        expire(&self.templates);
        expire(&self.prompts);
    }
}

fn expire<T>(slot: &Mutex<Option<Snapshot<T>>>) {
    if let Some(snapshot) = lock(slot).as_mut() {
        snapshot.metadata = request_refresh(snapshot.metadata);
    }
}

#[derive(Debug, Default)]
pub(crate) struct Invalidation {
    generation: AtomicU64,
    handled: AtomicU64,
}

impl Invalidation {
    pub(crate) fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(crate) fn pending(&self) -> bool {
        self.generation() != self.handled.load(Ordering::Acquire)
    }

    pub(crate) fn clear_through(&self, generation: u64) {
        if self.generation() == generation {
            self.handled.store(generation, Ordering::Release);
        }
    }
}

pub(crate) trait FeatureCatalog: Listed + Clone + Send + Sync + 'static {
    fn slot(catalogs: &FeatureCatalogs) -> &Mutex<Option<Snapshot<Self>>>;

    fn invalidation(client: &McpClient) -> &Invalidation;

    fn unavailable() -> McpError;
}

impl FeatureCatalog for Resource {
    fn slot(catalogs: &FeatureCatalogs) -> &Mutex<Option<Snapshot<Self>>> {
        &catalogs.resources
    }

    fn invalidation(client: &McpClient) -> &Invalidation {
        &client.resources_invalidation
    }

    fn unavailable() -> McpError {
        McpError::McpResourceCatalogUnavailable
    }
}

impl FeatureCatalog for ResourceTemplate {
    fn slot(catalogs: &FeatureCatalogs) -> &Mutex<Option<Snapshot<Self>>> {
        &catalogs.templates
    }

    fn invalidation(client: &McpClient) -> &Invalidation {
        &client.resources_invalidation
    }

    fn unavailable() -> McpError {
        McpError::McpResourceCatalogUnavailable
    }
}

impl FeatureCatalog for Prompt {
    fn slot(catalogs: &FeatureCatalogs) -> &Mutex<Option<Snapshot<Self>>> {
        &catalogs.prompts
    }

    fn invalidation(client: &McpClient) -> &Invalidation {
        &client.prompts_invalidation
    }

    fn unavailable() -> McpError {
        McpError::McpPromptCatalogUnavailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_freshness::Freshness;

    #[test]
    fn resource_invalidations_clear_only_through_the_generation_a_refresh_saw() {
        let invalidation = Invalidation::default();
        assert!(!invalidation.pending());
        invalidation.invalidate();
        let seen = invalidation.generation();
        invalidation.invalidate();
        invalidation.clear_through(seen);
        assert!(invalidation.pending());
        invalidation.clear_through(invalidation.generation());
        assert!(!invalidation.pending());
    }

    #[test]
    fn reloads_and_failures_keep_the_published_items() {
        let catalogs = FeatureCatalogs::default();
        let resource = |uri: &str| Resource {
            uri: uri.to_owned(),
            name: "a".to_owned(),
            title: None,
        };
        let items: Arc<[Resource]> = Arc::from(vec![resource("a://")]);
        catalogs.record_failed_refresh(&items, 10);
        assert!(catalogs.snapshot::<Resource>().is_none());
        catalogs.publish(Snapshot {
            items: Arc::clone(&items),
            metadata: SnapshotMetadata::fresh(u64::MAX),
        });
        catalogs.record_failed_refresh(&Arc::from(vec![resource("a://")]), 10);
        assert_eq!(
            catalogs.snapshot::<Resource>().unwrap().metadata.freshness,
            Freshness::Fresh
        );
        catalogs.record_failed_refresh(&items, 10);
        let failed = catalogs.snapshot::<Resource>().unwrap();
        assert!(Arc::ptr_eq(&failed.items, &items));
        assert_eq!(failed.metadata.freshness, Freshness::FailedRefresh);
        assert_eq!(failed.metadata.retry_at_ms, 110);
        catalogs.request_refresh();
        let requested = catalogs.snapshot::<Resource>().unwrap().metadata;
        assert_eq!(requested.freshness, Freshness::Stale);
        assert_eq!(requested.retry_at_ms, 0);
        assert!(catalogs.snapshot::<ResourceTemplate>().is_none());
    }

    #[test]
    fn prompt_catalogs_expire_on_reload_and_clear_on_reconnect() {
        let catalogs = FeatureCatalogs::default();
        let prompts: Arc<[Prompt]> = Arc::from(vec![Prompt {
            name: "review".to_owned(),
            title: None,
            description: None,
            arguments: Vec::new(),
        }]);
        catalogs.publish(Snapshot {
            items: Arc::clone(&prompts),
            metadata: SnapshotMetadata::fresh(u64::MAX),
        });
        catalogs.request_refresh();
        let requested = catalogs.snapshot::<Prompt>().unwrap();
        assert!(Arc::ptr_eq(&requested.items, &prompts));
        assert_eq!(requested.metadata.freshness, Freshness::Stale);
        catalogs.reset(ServerCapabilities::default());
        assert!(catalogs.snapshot::<Prompt>().is_none());
        assert!(!catalogs.advertises_prompts());
    }
}
