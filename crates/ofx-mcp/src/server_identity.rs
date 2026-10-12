use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_IDENTITY: AtomicU64 = AtomicU64::new(1);

pub(crate) fn allocate() -> u64 {
    NEXT_IDENTITY.fetch_add(1, Ordering::Relaxed)
}
