use ofx_text::PreparedQuery;

use crate::BoxFuture;

#[derive(Debug, Clone, Copy)]
pub struct McpSearchRequest<'a> {
    pub query: &'a PreparedQuery,
    pub server: Option<&'a str>,
    pub result_bytes: Option<usize>,
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
    fn search_tools<'a>(&'a self, request: McpSearchRequest<'a>) -> BoxFuture<'a, McpSearchResult>;
}
