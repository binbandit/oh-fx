use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::content::Kind;

const TTL: Duration = Duration::from_mins(15);
const ENTRY_OVERHEAD_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Page {
    pub(crate) final_url: String,
    pub(crate) status: u16,
    pub(crate) mime_type: String,
    pub(crate) kind: Kind,
    pub(crate) converted_content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Limits {
    entries: usize,
    converted_bytes: usize,
    metadata_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: 1024,
            converted_bytes: 50 * 1024 * 1024,
            metadata_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct FetchCache {
    limits: Limits,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    entries: Vec<Entry>,
    converted_bytes: usize,
    metadata_bytes: usize,
    lru_tick: u64,
}

#[derive(Debug)]
struct Entry {
    submitted_url: String,
    page: Page,
    expires_at: Instant,
    last_used: u64,
    metadata_weight: usize,
}

impl FetchCache {
    pub(crate) fn lookup(&self, submitted_url: &str, now: Instant) -> Option<Page> {
        let mut state = self.lock();
        state.evict_expired(now);
        let index = state.position(submitted_url)?;
        state.lru_tick += 1;
        let tick = state.lru_tick;
        let entry = &mut state.entries[index];
        entry.last_used = tick;
        Some(entry.page.clone())
    }

    pub(crate) fn insert(&self, submitted_url: &str, page: Page, now: Instant) {
        let limits = self.limits;
        let mut state = self.lock();
        state.evict_expired(now);
        if let Some(index) = state.position(submitted_url) {
            state.remove(index);
        }
        let metadata_weight = ENTRY_OVERHEAD_BYTES
            + submitted_url.len()
            + page.final_url.len()
            + page.mime_type.len();
        if metadata_weight > limits.metadata_bytes
            || page.converted_content.len() > limits.converted_bytes
        {
            return;
        }
        state.lru_tick += 1;
        state.converted_bytes += page.converted_content.len();
        state.metadata_bytes += metadata_weight;
        let last_used = state.lru_tick;
        state.entries.push(Entry {
            submitted_url: submitted_url.to_owned(),
            page,
            expires_at: now + TTL,
            last_used,
            metadata_weight,
        });
        while state.entries.len() > limits.entries
            || state.converted_bytes > limits.converted_bytes
            || state.metadata_bytes > limits.metadata_bytes
        {
            let Some(oldest) = state
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(index, _)| index)
            else {
                return;
            };
            state.remove(oldest);
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    fn position(&self, submitted_url: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.submitted_url == submitted_url)
    }

    fn evict_expired(&mut self, now: Instant) {
        let mut index = 0;
        while index < self.entries.len() {
            if self.entries[index].expires_at > now {
                index += 1;
            } else {
                self.remove(index);
            }
        }
    }

    fn remove(&mut self, index: usize) {
        let entry = self.entries.swap_remove(index);
        self.converted_bytes -= entry.page.converted_content.len();
        self.metadata_bytes -= entry.metadata_weight;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(content: &str) -> Page {
        Page {
            final_url: "https://example.com/docs".to_owned(),
            status: 200,
            mime_type: "text/plain".to_owned(),
            kind: Kind::Text,
            converted_content: content.to_owned(),
        }
    }

    fn cache(
        max_entries: usize,
        max_converted_bytes: usize,
        max_metadata_bytes: usize,
    ) -> FetchCache {
        FetchCache {
            limits: Limits {
                entries: max_entries,
                converted_bytes: max_converted_bytes,
                metadata_bytes: max_metadata_bytes,
            },
            state: Mutex::default(),
        }
    }

    #[test]
    fn expires_at_fifteen_minutes_and_evicts_by_converted_bytes() {
        let cache = cache(1024, 5, 4096);
        let start = Instant::now();
        cache.insert("https://a.example/", page("abc"), start);
        assert_eq!(
            cache.lookup("https://a.example/", start + Duration::from_secs(899)),
            Some(page("abc"))
        );
        assert_eq!(
            cache.lookup("https://a.example/", start + Duration::from_mins(15)),
            None
        );
        cache.insert("https://a.example/", page("abc"), start);
        cache.insert("https://b.example/", page("def"), start);
        assert_eq!(cache.lookup("https://a.example/", start), None);
        assert_eq!(cache.lookup("https://b.example/", start), Some(page("def")));
    }

    #[test]
    fn evicts_the_least_recently_used_entry_and_caps_entry_count_and_metadata() {
        let cache = cache(2, 1024, 4096);
        let now = Instant::now();
        cache.insert("https://a.example/", page("a"), now);
        cache.insert("https://b.example/", page("b"), now);
        assert!(cache.lookup("https://a.example/", now).is_some());
        cache.insert("https://c.example/", page("c"), now);
        assert!(cache.lookup("https://a.example/", now).is_some());
        assert!(cache.lookup("https://b.example/", now).is_none());
        assert!(cache.lookup("https://c.example/", now).is_some());

        let small = cache_with_metadata_cap(80);
        small.insert("https://a.example/", page("a"), now);
        assert!(small.lookup("https://a.example/", now).is_none());
    }

    #[test]
    fn replacing_an_entry_keeps_one_copy_and_oversized_pages_are_not_kept() {
        let cache = cache(1024, 4, 4096);
        let now = Instant::now();
        cache.insert("https://a.example/", page("one"), now);
        cache.insert("https://a.example/", page("two"), now);
        assert_eq!(cache.lookup("https://a.example/", now), Some(page("two")));
        cache.insert("https://a.example/", page("too long"), now);
        assert_eq!(cache.lookup("https://a.example/", now), None);
    }

    fn cache_with_metadata_cap(max_metadata_bytes: usize) -> FetchCache {
        cache(1024, 1024, max_metadata_bytes)
    }
}
