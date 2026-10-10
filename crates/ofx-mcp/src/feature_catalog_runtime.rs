use std::sync::Arc;

use tokio::time::Instant;

use crate::catalog_freshness::{RefreshAction, SnapshotMetadata, decide_refresh};
use crate::error::McpError;
use crate::feature_catalog::{FeatureCatalog, Snapshot};
use crate::features::resources::{Catalog, CatalogBuilder, Limits, parse_page};
use crate::mcp_contract::TransportType;
use crate::operation_control::monotonic_millis;
use crate::protocol_messages::build_list_request;
use crate::server_connection::McpClient;
use crate::server_lifecycle::{Lifecycle, Server};
use crate::timing::timeout_at;
use crate::transport::{McpTransport, TransportRequest};

const FEATURE_RESPONSE_FRAME_CAP_BYTES: usize = 4 * 1024 * 1024;

impl Server {
    pub(crate) async fn feature_catalog<T: FeatureCatalog>(
        self: &Arc<Self>,
        deadline: Instant,
    ) -> Result<Arc<[T]>, McpError> {
        loop {
            let _refreshing = timeout_at(deadline, self.features.refresh.lock())
                .await
                .map_err(|_| McpError::McpRequestTimedOut)?;
            let client = match self.lifecycle() {
                Lifecycle::Ready(client) => Some(client),
                Lifecycle::Idle | Lifecycle::Starting | Lifecycle::Failed(_) => None,
            };
            let current = self.features.snapshot::<T>();
            let invalidated = client
                .as_ref()
                .is_some_and(|client| T::invalidation(client).pending());
            if let Some(current) = &current
                && decide_refresh(current.metadata, monotonic_millis(), invalidated)
                    != RefreshAction::Refresh
            {
                return Ok(Arc::clone(&current.items));
            }
            let Some(client) = client else {
                return self.keep_after_failure(current, McpError::McpConnectionClosed);
            };
            if self.config.transport == TransportType::Stdio && !client.is_running() {
                match self.running_client().await {
                    Ok(_) => continue,
                    Err(failure) => return self.keep_after_failure(current, failure.into_error()),
                }
            }
            let invalidation = T::invalidation(&client);
            let generation = invalidation.generation();
            return match fetch::<T>(&client, deadline).await {
                Ok(catalog) => {
                    let items: Arc<[T]> = catalog.items.into();
                    if matches!(self.lifecycle(), Lifecycle::Ready(published) if Arc::ptr_eq(&published, &client))
                    {
                        self.features.publish(Snapshot {
                            items: Arc::clone(&items),
                            metadata: SnapshotMetadata::fresh(catalog.expires_at_ms),
                        });
                        invalidation.clear_through(generation);
                    }
                    Ok(items)
                }
                Err(error) => self.keep_after_failure(current, error),
            };
        }
    }

    fn keep_after_failure<T: FeatureCatalog>(
        &self,
        current: Option<Snapshot<T>>,
        error: McpError,
    ) -> Result<Arc<[T]>, McpError> {
        let current = current.ok_or(error)?;
        self.features
            .record_failed_refresh(&current.items, monotonic_millis());
        Ok(current.items)
    }
}

async fn fetch<T: FeatureCatalog>(
    client: &McpClient,
    deadline: Instant,
) -> Result<Catalog<T>, McpError> {
    let limits = Limits::default();
    let mut builder = CatalogBuilder::default();
    let mut cursor = None;
    loop {
        let id = client.transport.next_request_id()?;
        let body = build_list_request(id, T::LIST_METHOD, cursor.as_deref());
        let response = client
            .transport
            .request(TransportRequest::new(
                id,
                body,
                FEATURE_RESPONSE_FRAME_CAP_BYTES,
                deadline,
            ))
            .await?;
        let received_at_ms = monotonic_millis();
        let page = parse_page::<T>(&response, limits)?;
        cursor = builder.append_page(page, received_at_ms, limits)?;
        if cursor.is_none() {
            return builder.finish();
        }
    }
}
