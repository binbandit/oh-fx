use std::pin::pin;
use std::sync::Arc;

use tokio::time::Instant;

use crate::catalog_freshness::{
    RefreshAction, SnapshotMetadata, begin_refresh, decide_refresh, failed_refresh, request_refresh,
};
use crate::error::McpError;
use crate::features::tools::ToolCatalog;
use crate::operation_control::monotonic_millis;
use crate::server_connection::{McpClient, lock};
use crate::server_transport::discover_tools;
use crate::timing::timeout_at;

pub(crate) struct Refreshed {
    pub(crate) catalog: Arc<ToolCatalog>,
    pub(crate) replaced: bool,
    pub(crate) in_flight: bool,
    pub(crate) authentication_required: bool,
}

impl McpClient {
    pub(crate) fn request_tool_refresh(&self) {
        let mut snapshot = lock(&self.tools);
        snapshot.metadata = request_refresh(snapshot.metadata);
    }

    pub(crate) async fn settled_tools(&self, deadline: Instant) -> Result<Refreshed, McpError> {
        loop {
            let mut settled = pin!(self.tools_settled.notified());
            settled.as_mut().enable();
            let refreshed = self.refresh_tools(deadline).await;
            if !refreshed.in_flight || !self.tools_invalidation.pending() {
                return Ok(refreshed);
            }
            if timeout_at(deadline, settled).await.is_err() {
                return Err(McpError::McpRequestTimedOut);
            }
        }
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
                    in_flight: action == RefreshAction::AlreadyRefreshing,
                    authentication_required: false,
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
        let listing = match discover_tools(&self.transport, deadline, |error| error).await {
            Ok(listing) => listing,
            Err(error) => {
                return Refreshed {
                    catalog: self.tool_catalog(),
                    replaced: false,
                    in_flight: false,
                    authentication_required: error == McpError::McpAuthenticationRequired,
                };
            }
        };
        pending.source = None;
        let mut snapshot = lock(&self.tools);
        let replaced = snapshot.catalog.tools != listing.catalog.tools;
        if replaced {
            snapshot.catalog = Arc::new(listing.catalog);
        }
        snapshot.metadata = SnapshotMetadata::fresh(listing.expires_at_ms);
        self.tools_invalidation.clear_through(invalidation);
        Refreshed {
            catalog: Arc::clone(&snapshot.catalog),
            replaced,
            in_flight: false,
            authentication_required: false,
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
        self.client.tools_settled.notify_waiters();
    }
}

#[cfg(test)]
mod tests;
