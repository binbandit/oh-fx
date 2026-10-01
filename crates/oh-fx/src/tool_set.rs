use std::path::Path;
use std::sync::Arc;

use ofx_contract::Tool;
use ofx_tools::{EditFile, GlobFiles, GrepFiles, ReadFile, WriteFile};

pub(crate) fn ask_tools(workspace_root: &Path) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFile::new(workspace_root)),
        Arc::new(GlobFiles::new(workspace_root)),
        Arc::new(GrepFiles::new(workspace_root)),
        Arc::new(EditFile::new(workspace_root)),
        Arc::new(WriteFile::new(workspace_root)),
    ]
}
