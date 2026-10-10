const RETRY_INITIAL_MS: u64 = 100;
const RETRY_MAX_MS: u64 = 5_000;
const RETRY_MAX_ATTEMPT: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheScope {
    Private,
    Public,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Freshness {
    Fresh,
    Stale,
    FailedRefresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SnapshotMetadata {
    pub(crate) expires_at_ms: u64,
    pub(crate) freshness: Freshness,
    pub(crate) refresh_attempt: u8,
    pub(crate) retry_at_ms: u64,
}

impl SnapshotMetadata {
    pub(crate) fn fresh(expires_at_ms: u64) -> Self {
        Self {
            expires_at_ms,
            freshness: Freshness::Fresh,
            refresh_attempt: 0,
            retry_at_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshAction {
    Hit,
    Refresh,
    RetryLater,
}

pub(crate) fn page_expiry(received_at_ms: u64, ttl_ms: Option<u64>) -> u64 {
    received_at_ms.saturating_add(ttl_ms.unwrap_or(u64::MAX))
}

pub(crate) fn earliest_expiry(current: Option<u64>, page_expiry_ms: u64) -> u64 {
    current.map_or(page_expiry_ms, |expiry| expiry.min(page_expiry_ms))
}

pub(crate) fn decide_refresh(
    metadata: SnapshotMetadata,
    now_ms: u64,
    invalidated: bool,
) -> RefreshAction {
    if metadata.freshness == Freshness::FailedRefresh && now_ms < metadata.retry_at_ms {
        return RefreshAction::RetryLater;
    }
    if invalidated {
        return RefreshAction::Refresh;
    }
    if metadata.freshness == Freshness::Fresh && now_ms < metadata.expires_at_ms {
        return RefreshAction::Hit;
    }
    RefreshAction::Refresh
}

pub(crate) fn effective_freshness(
    metadata: SnapshotMetadata,
    now_ms: u64,
    invalidated: bool,
) -> Freshness {
    match metadata.freshness {
        Freshness::FailedRefresh => Freshness::FailedRefresh,
        Freshness::Fresh | Freshness::Stale => {
            if invalidated || now_ms >= metadata.expires_at_ms {
                Freshness::Stale
            } else {
                Freshness::Fresh
            }
        }
    }
}

pub(crate) fn request_refresh(metadata: SnapshotMetadata) -> SnapshotMetadata {
    SnapshotMetadata {
        expires_at_ms: 0,
        retry_at_ms: 0,
        freshness: Freshness::Stale,
        ..metadata
    }
}

pub(crate) fn retry_delay_ms(attempt: u8) -> u64 {
    let mut delay = RETRY_INITIAL_MS;
    for _ in 0..attempt.min(RETRY_MAX_ATTEMPT) {
        delay = delay.saturating_mul(2).min(RETRY_MAX_MS);
    }
    delay
}

pub(crate) fn failed_refresh(metadata: SnapshotMetadata, now_ms: u64) -> SnapshotMetadata {
    SnapshotMetadata {
        freshness: Freshness::FailedRefresh,
        refresh_attempt: metadata
            .refresh_attempt
            .saturating_add(1)
            .min(RETRY_MAX_ATTEMPT),
        retry_at_ms: now_ms.saturating_add(retry_delay_ms(metadata.refresh_attempt)),
        ..metadata
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_refresh_preserves_the_last_valid_snapshot() {
        let current = SnapshotMetadata::fresh(200);
        let failed = failed_refresh(current, 250);
        assert_eq!(failed.freshness, Freshness::FailedRefresh);
        assert_eq!(failed.expires_at_ms, current.expires_at_ms);
        assert_eq!(failed.refresh_attempt, 1);
        assert_eq!(failed.retry_at_ms, 350);
        assert_eq!(decide_refresh(failed, 349, true), RefreshAction::RetryLater);
        assert_eq!(decide_refresh(failed, 350, false), RefreshAction::Refresh);
        assert_eq!(
            effective_freshness(failed, 0, false),
            Freshness::FailedRefresh
        );
    }

    #[test]
    fn retries_back_off_from_100_ms_to_5_seconds() {
        let delays: Vec<u64> = (0..=9).map(retry_delay_ms).collect();
        assert_eq!(
            delays,
            [100, 200, 400, 800, 1_600, 3_200, 5_000, 5_000, 5_000, 5_000]
        );
        let mut metadata = SnapshotMetadata::fresh(0);
        for attempt in 1..=10 {
            metadata = failed_refresh(metadata, 0);
            assert_eq!(metadata.refresh_attempt, attempt.min(RETRY_MAX_ATTEMPT));
        }
    }

    #[test]
    fn expiry_and_retry_arithmetic_saturate() {
        assert_eq!(page_expiry(u64::MAX - 1, Some(10)), u64::MAX);
        let metadata = SnapshotMetadata {
            refresh_attempt: RETRY_MAX_ATTEMPT,
            ..SnapshotMetadata::fresh(0)
        };
        let failed = failed_refresh(metadata, u64::MAX - 1);
        assert_eq!(failed.retry_at_ms, u64::MAX);
        assert_eq!(failed.refresh_attempt, RETRY_MAX_ATTEMPT);
    }

    #[test]
    fn missing_lifetimes_never_expire_and_present_ones_keep_clock_boundaries() {
        assert_eq!(page_expiry(4_000, None), u64::MAX);
        assert_eq!(page_expiry(2_000, Some(0)), 2_000);
        let metadata = SnapshotMetadata::fresh(500);
        assert_eq!(decide_refresh(metadata, 499, false), RefreshAction::Hit);
        assert_eq!(decide_refresh(metadata, 500, false), RefreshAction::Refresh);
        assert_eq!(effective_freshness(metadata, 499, false), Freshness::Fresh);
        assert_eq!(effective_freshness(metadata, 500, false), Freshness::Stale);
    }

    #[test]
    fn paginated_cache_expiry_uses_each_page_receive_time() {
        let first = page_expiry(1_000, Some(100));
        assert_eq!(first, 1_100);
        assert_eq!(
            earliest_expiry(Some(first), page_expiry(1_050, Some(500))),
            first
        );
        assert_eq!(earliest_expiry(None, first), first);
    }

    #[test]
    fn invalidation_and_explicit_requests_force_a_refresh() {
        let metadata = SnapshotMetadata::fresh(u64::MAX);
        assert_eq!(decide_refresh(metadata, 0, true), RefreshAction::Refresh);
        assert_eq!(effective_freshness(metadata, 0, true), Freshness::Stale);
        let requested = request_refresh(failed_refresh(metadata, 0));
        assert_eq!(requested.freshness, Freshness::Stale);
        assert_eq!(requested.retry_at_ms, 0);
        assert_eq!(decide_refresh(requested, 0, false), RefreshAction::Refresh);
    }
}
