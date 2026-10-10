#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpServersSection {
    pub text: String,
    pub change_notice: Option<String>,
    pub notice: Option<String>,
}

pub trait McpServersCatalog: Send + Sync {
    fn section(&self) -> McpServersSection;
}
