use std::pin::pin;
use std::sync::Arc;

use tokio::time::Instant;

use crate::catalog_freshness::{
    Freshness, RefreshAction, SnapshotMetadata, begin_refresh, decide_refresh, effective_freshness,
    failed_refresh, request_refresh,
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
    pub(crate) lost_authentication: Option<McpError>,
    pub(crate) outcome: ToolRefresh,
}

pub(crate) enum ToolRefresh {
    Skipped {
        action: RefreshAction,
        freshness: Freshness,
        generation: u64,
    },
    Unchanged {
        generation: u64,
        fetched_at_ms: u64,
        expires_at_ms: u64,
    },
    Replaced {
        generation: u64,
        tools: usize,
    },
    Failed {
        retry_at_ms: u64,
        preserved_tools: usize,
        error: McpError,
    },
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
            let now_ms = monotonic_millis();
            let invalidated = self.tools_invalidation.pending();
            let action = decide_refresh(snapshot.metadata, now_ms, invalidated);
            if action != RefreshAction::Refresh {
                return Refreshed {
                    catalog: Arc::clone(&snapshot.catalog),
                    replaced: false,
                    in_flight: action == RefreshAction::AlreadyRefreshing,
                    lost_authentication: None,
                    outcome: ToolRefresh::Skipped {
                        action,
                        freshness: effective_freshness(snapshot.metadata, now_ms, invalidated),
                        generation: snapshot.generation,
                    },
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
                let failed = pending.fail();
                let catalog = self.tool_catalog();
                return Refreshed {
                    replaced: false,
                    in_flight: false,
                    lost_authentication: self
                        .transport
                        .lost_authentication(&error)
                        .then(|| error.clone()),
                    outcome: ToolRefresh::Failed {
                        retry_at_ms: failed.retry_at_ms,
                        preserved_tools: catalog.tools.len(),
                        error,
                    },
                    catalog,
                };
            }
        };
        pending.source = None;
        let mut snapshot = lock(&self.tools);
        let replaced = snapshot.catalog.tools != listing.catalog.tools;
        if replaced {
            snapshot.catalog = Arc::new(listing.catalog);
            snapshot.generation += 1;
        }
        snapshot.metadata = SnapshotMetadata::fresh(listing.expires_at_ms);
        self.tools_invalidation.clear_through(invalidation);
        let outcome = if replaced {
            ToolRefresh::Replaced {
                generation: snapshot.generation,
                tools: snapshot.catalog.tools.len(),
            }
        } else {
            ToolRefresh::Unchanged {
                generation: snapshot.generation,
                fetched_at_ms: monotonic_millis(),
                expires_at_ms: listing.expires_at_ms,
            }
        };
        Refreshed {
            catalog: Arc::clone(&snapshot.catalog),
            replaced,
            in_flight: false,
            lost_authentication: None,
            outcome,
        }
    }
}

struct PendingRefresh<'a> {
    client: &'a McpClient,
    source: Option<SnapshotMetadata>,
}

impl PendingRefresh<'_> {
    fn fail(&mut self) -> SnapshotMetadata {
        let mut snapshot = lock(&self.client.tools);
        if let Some(source) = self.source.take() {
            snapshot.metadata = failed_refresh(source, monotonic_millis());
        }
        snapshot.metadata
    }
}

impl Drop for PendingRefresh<'_> {
    fn drop(&mut self) {
        if self.source.is_some() {
            self.fail();
        }
        self.client.tools_settled.notify_waiters();
    }
}

#[cfg(test)]
mod tests;
