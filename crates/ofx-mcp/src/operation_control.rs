use std::sync::OnceLock;
use std::time::Instant;

pub(crate) fn monotonic_millis() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let origin = *ORIGIN.get_or_init(Instant::now);
    u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_never_runs_backwards() {
        let first = monotonic_millis();
        let second = monotonic_millis();
        assert!(second >= first);
    }
}
