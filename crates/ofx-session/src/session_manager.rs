mod catalog;
mod crc32c;
mod fields;
mod frame;
mod json;
mod storage;
#[cfg(test)]
pub(crate) mod test_support;

use ofx_config::PrivateDir;

pub(crate) use catalog::{ListCursor, Page, Summary};
pub(crate) use fields::full_string;
pub(crate) use storage::IoFault;

pub(crate) struct Manager {
    root: PrivateDir,
}

impl Manager {
    pub(crate) fn new(root: PrivateDir) -> Self {
        Self { root }
    }

    pub(crate) fn list(&self, cursor: Option<&ListCursor>, limit: usize) -> Result<Page, IoFault> {
        catalog::list(&self.root, cursor, limit.max(1))
    }
}
