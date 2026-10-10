use std::sync::Arc;

use tokio::time::Instant;

use crate::catalog_freshness::{
    RefreshAction, SnapshotMetadata, begin_refresh, decide_refresh, failed_refresh, request_refresh,
};
use crate::features::tools::ToolCatalog;
use crate::operation_control::monotonic_millis;
use crate::server_connection::{McpClient, lock};
use crate::server_transport::discover_tools;

pub(crate) struct Refreshed {
    pub(crate) catalog: Arc<ToolCatalog>,
    pub(crate) replaced: bool,
}

impl McpClient {
    pub(crate) fn tools_need_refresh(&self) -> bool {
        decide_refresh(
            lock(&self.tools).metadata,
            monotonic_millis(),
            self.tools_invalidation.pending(),
        ) == RefreshAction::Refresh
    }

    pub(crate) fn request_tool_refresh(&self) {
        let mut snapshot = lock(&self.tools);
        snapshot.metadata = request_refresh(snapshot.metadata);
    }

    pub(crate) async fn refresh_tools(&self, deadline: Instant) -> Refreshed {
        self.receive_pending_notifications();
        let invalidation = self.tools_invalidation.generation();
        let source = {
            let mut snapshot = lock(&self.tools);
            let action = decide_refresh(
                snapshot.metadata,
                monotonic_millis(),
                self.tools_invalidation.pending(),
            );
            if action != RefreshAction::Refresh {
                return Refreshed {
                    catalog: Arc::clone(&snapshot.catalog),
                    replaced: false,
                };
            }
            let source = snapshot.metadata;
            snapshot.metadata = begin_refresh(source);
            source
        };
        let mut pending = PendingRefresh {
            client: self,
            source: Some(source),
        };
        let listed = discover_tools(&self.transport, deadline, |error| error).await;
        pending.source = None;
        let mut snapshot = lock(&self.tools);
        let Ok(listing) = listed else {
            snapshot.metadata = failed_refresh(source, monotonic_millis());
            return Refreshed {
                catalog: Arc::clone(&snapshot.catalog),
                replaced: false,
            };
        };
        let replaced = snapshot.catalog.tools != listing.catalog.tools;
        if replaced {
            snapshot.catalog = Arc::new(listing.catalog);
        }
        snapshot.metadata = SnapshotMetadata::fresh(listing.expires_at_ms);
        self.tools_invalidation.clear_through(invalidation);
        Refreshed {
            catalog: Arc::clone(&snapshot.catalog),
            replaced,
        }
    }
}

struct PendingRefresh<'a> {
    client: &'a McpClient,
    source: Option<SnapshotMetadata>,
}

impl Drop for PendingRefresh<'_> {
    fn drop(&mut self) {
        if let Some(source) = self.source.take() {
            let mut snapshot = lock(&self.client.tools);
            snapshot.metadata = failed_refresh(source, monotonic_millis());
        }
    }
}

#[cfg(test)]
mod tests;
