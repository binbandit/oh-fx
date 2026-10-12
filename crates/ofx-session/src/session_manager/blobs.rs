use std::io::Read;
use std::os::fd::AsFd;

use sha2::{Digest, Sha256};

use super::schema::valid_blob_hash;
use super::storage::{self, IoFault, StorageError};

const BLOBS_DIR: &str = "blobs";
const MAX_BLOB_BYTES: u64 = 512 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlobError {
    NotFound,
    Corrupt,
    Fault(IoFault),
}

impl From<StorageError> for BlobError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::NotFound => Self::NotFound,
            StorageError::Fault(fault) => Self::Fault(fault),
        }
    }
}

pub(crate) fn read_blob(session: impl AsFd, hash: &str) -> Result<Vec<u8>, BlobError> {
    if !valid_blob_hash(hash) {
        return Err(BlobError::NotFound);
    }
    let blobs = storage::open_dir(session, BLOBS_DIR)?;
    let blob = storage::open_file(&blobs, hash)?;
    let len = blob.metadata().map_err(StorageError::from)?.len();
    if len > MAX_BLOB_BYTES {
        return Err(BlobError::Corrupt);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or_default());
    blob.take(len)
        .read_to_end(&mut bytes)
        .map_err(StorageError::from)?;
    if bytes.len() as u64 != len {
        return Err(BlobError::Fault(IoFault::Io));
    }
    if ofx_text::lowercase_hex(&Sha256::digest(&bytes)) != hash {
        return Err(BlobError::Corrupt);
    }
    Ok(bytes)
}
