use std::sync::Arc;

use ofx_contract::{McpMenuArgument, McpMenuCompletionTarget, UiEvent};
use ofx_mcp::{CompletionArgument, McpRuntime};

use super::super::McpHost;

const EMPTY: &str = "McpCompletionEmpty";

impl McpHost {
    pub(super) fn complete_menu_argument(
        &self,
        generation: u64,
        target: McpMenuCompletionTarget,
        argument: McpMenuArgument,
        context: Vec<McpMenuArgument>,
    ) {
        let runtime = Arc::clone(&self.runtime);
        self.start_menu_operation(generation, "completion", async move {
            complete(&runtime, target, &argument, &context)
                .await
                .map(|value| UiEvent::McpMenuCompleted { generation, value })
        });
    }
}

async fn complete(
    runtime: &McpRuntime,
    target: McpMenuCompletionTarget,
    argument: &McpMenuArgument,
    context: &[McpMenuArgument],
) -> Result<String, String> {
    let current = completion_argument(argument);
    let context: Vec<CompletionArgument<'_>> = context.iter().map(completion_argument).collect();
    let completion = match target {
        McpMenuCompletionTarget::Prompt { server, name } => {
            runtime
                .complete_prompt_argument(&server, &name, current, &context)
                .await
        }
        McpMenuCompletionTarget::Resource {
            server,
            uri_template,
        } => {
            runtime
                .complete_resource_template_argument(&server, &uri_template, current, &context)
                .await
        }
    }
    .map_err(|error| error.to_string())?;
    completion
        .values
        .into_iter()
        .next()
        .ok_or_else(|| EMPTY.to_owned())
}

fn completion_argument(argument: &McpMenuArgument) -> CompletionArgument<'_> {
    CompletionArgument {
        name: &argument.name,
        value: &argument.value,
    }
}

#[cfg(test)]
mod tests;
