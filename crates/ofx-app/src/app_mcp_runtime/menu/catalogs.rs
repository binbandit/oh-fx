use std::sync::Arc;

use ofx_contract::{
    McpMenuCancel, McpMenuCatalog, McpMenuCatalogRequest, McpMenuOutcome, McpMenuPrompt,
    McpMenuResource, UiEvent,
};
use ofx_mcp::{McpError, McpRuntime, PromptSummary, ResourceSummary};

use super::super::{McpHost, MenuOperation, lock};
use super::settled;

impl McpHost {
    pub(super) fn load_menu_catalog(&self, generation: u64, request: McpMenuCatalogRequest) {
        let runtime = Arc::clone(&self.runtime);
        let emit = Arc::clone(&self.emit);
        let profile = self.profile_config();
        let task = tokio::spawn(async move {
            emit(match catalog(&runtime, request).await {
                Ok(catalog) => UiEvent::McpMenuLoaded {
                    generation,
                    catalog,
                },
                Err(error) => settled(
                    &runtime,
                    profile,
                    generation,
                    McpMenuOutcome::Failed,
                    format!("MCP catalog failed: {error}"),
                ),
            });
        });
        self.discard_menu_operation("superseded");
        *lock(&self.menu_operation) = Some(MenuOperation { generation, task });
    }

    pub(super) fn cancel_menu_operation(&self, generation: u64, reason: McpMenuCancel) {
        if lock(&self.menu_operation)
            .as_ref()
            .is_some_and(|operation| operation.generation == generation)
        {
            self.discard_menu_operation(match reason {
                McpMenuCancel::Back => "menu_back",
                McpMenuCancel::Closed => "menu_closed",
            });
        }
    }

    pub(in crate::app_mcp_runtime) fn discard_menu_operation(&self, reason: &str) {
        let Some(operation) = lock(&self.menu_operation).take() else {
            return;
        };
        if !operation.task.is_finished() {
            ofx_trace::log(
                "mcp",
                format_args!(
                    "discarding pending menu operation generation={} reason={reason}",
                    operation.generation
                ),
            );
        }
        operation.task.abort();
    }
}

async fn catalog(
    runtime: &McpRuntime,
    request: McpMenuCatalogRequest,
) -> Result<McpMenuCatalog, McpError> {
    match request {
        McpMenuCatalogRequest::Tools => Ok(McpMenuCatalog::Tools(runtime.menu_tool_names())),
        McpMenuCatalogRequest::Resources(server) => {
            let listed = async {
                let resources = runtime.list_resources(&server, false).await?;
                let templates = runtime.list_resources(&server, true).await?;
                Ok::<_, McpError>(
                    resources
                        .into_iter()
                        .map(|summary| resource(summary, false))
                        .chain(templates.into_iter().map(|summary| resource(summary, true)))
                        .collect(),
                )
            };
            unsupported_as_empty(listed.await, &McpError::McpResourcesUnsupported)
                .map(McpMenuCatalog::Resources)
        }
        McpMenuCatalogRequest::Prompts(server) => {
            let listed = runtime
                .list_prompts(&server)
                .await
                .map(|prompts| prompts.into_iter().map(prompt).collect());
            unsupported_as_empty(listed, &McpError::McpPromptsUnsupported)
                .map(McpMenuCatalog::Prompts)
        }
    }
}

fn unsupported_as_empty<T>(
    listed: Result<Vec<T>, McpError>,
    unsupported: &McpError,
) -> Result<Vec<T>, McpError> {
    match listed {
        Err(error) if &error == unsupported => Ok(Vec::new()),
        listed => listed,
    }
}

fn resource(summary: ResourceSummary, template: bool) -> McpMenuResource {
    McpMenuResource {
        uri: summary.identity,
        name: summary.name,
        title: summary.title,
        description: summary.description,
        template,
    }
}

fn prompt(summary: PromptSummary) -> McpMenuPrompt {
    McpMenuPrompt {
        name: summary.name,
        title: summary.title,
        description: summary.description,
    }
}

#[cfg(test)]
mod tests;
