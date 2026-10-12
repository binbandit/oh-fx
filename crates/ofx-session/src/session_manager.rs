mod blobs;
mod catalog;
mod crc32c;
mod fields;
mod fold;
mod frame;
mod json;
mod log;
mod schema;
mod storage;
mod summary;
#[cfg(test)]
pub(crate) mod test_support;
mod verify;

use ofx_config::PrivateDir;

pub(crate) use blobs::BlobError;
pub(crate) use catalog::{ListCursor, Page, Summary};
pub(crate) use fields::{BadBody, Fields, full_string};
pub(crate) use fold::State;
pub(crate) use schema::{Body, Reason, Role, SetKey};
pub(crate) use storage::IoFault;
pub(crate) use summary::{OpenError, Peeked};
pub(crate) use verify::{Verified, VerifyError};

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

    pub(crate) fn peek(&self, id: &str) -> Result<Peeked, OpenError> {
        summary::read_summary(&self.root, id)
    }

    pub(crate) fn read_forward<E>(
        &self,
        id: &str,
        visit: impl FnMut(u64, Option<Body<'_>>) -> Result<(), E>,
    ) -> Result<Result<bool, E>, OpenError> {
        summary::read_forward(&self.root, id, visit)
    }

    pub(crate) fn get_blob(&self, id: &str, hash: &str) -> Result<Vec<u8>, BlobError> {
        if !schema::valid_id(id) {
            return Err(BlobError::NotFound);
        }
        let session = storage::open_dir(&self.root, id)?;
        blobs::read_blob(&session, hash)
    }

    pub(crate) fn verify(&self, id: &str) -> Result<Verified, VerifyError> {
        if !schema::valid_id(id) {
            return Err(VerifyError::InvalidArgument);
        }
        verify::verify_session(&self.root, id)
    }
}
