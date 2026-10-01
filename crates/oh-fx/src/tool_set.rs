use std::path::Path;
use std::sync::Arc;

use ofx_contract::Tool;
use ofx_tools::ReadFile;

pub(crate) fn ask_tools(workspace_root: &Path) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ReadFile::new(workspace_root))]
}
