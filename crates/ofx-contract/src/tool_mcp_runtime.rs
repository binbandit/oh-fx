use std::sync::Arc;

use ofx_text::PreparedQuery;

use crate::BoxFuture;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpSearchHost {
    Ask { result_bytes: usize },
    Interactive,
}

#[derive(Debug, Clone)]
pub struct McpSearchRequest {
    pub query: Arc<PreparedQuery>,
    pub server: Option<String>,
    pub host: McpSearchHost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSearchResult {
    pub model_output: String,
    pub notice: Option<String>,
}

impl McpSearchResult {
    #[must_use]
    pub fn plain(model_output: impl Into<String>) -> Self {
        Self {
            model_output: model_output.into(),
            notice: None,
        }
    }
}

pub trait McpToolSearch: Send + Sync {
    fn search_tools(
        self: Arc<Self>,
        request: McpSearchRequest,
    ) -> BoxFuture<'static, McpSearchResult>;
}
