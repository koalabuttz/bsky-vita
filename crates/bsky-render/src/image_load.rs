//! Shared download/decode state. A failed screen must not poison a URL forever.
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct ImageLoads {
    entries: HashMap<String, Attempt>,
}

struct Attempt {
    failures: u32,
    pending: bool,
    retry_at: Option<Instant>,
    updated: Instant,
}

impl ImageLoads {
    pub fn request(&mut self, url: &str, now: Instant) -> bool {
        // Bound failure history; never remove a request that is still in flight.
        if !self.entries.contains_key(url) && self.entries.len() >= 256 {
            if let Some(oldest) = self
                .entries
                .iter()
                .filter(|(_, a)| !a.pending)
                .min_by_key(|(_, a)| a.updated)
                .map(|(url, _)| url.clone())
            {
                self.entries.remove(&oldest);
            } else {
                return false;
            }
        }
        let attempt = self.entries.entry(url.into()).or_insert(Attempt {
            failures: 0,
            pending: false,
            retry_at: Some(now),
            updated: now,
        });
        if attempt.pending || !attempt.retry_at.is_some_and(|deadline| now >= deadline) {
            return false;
        }
        attempt.pending = true;
        attempt.updated = now;
        true
    }

    pub fn failed(&mut self, url: &str, retryable: bool, now: Instant) {
        if let Some(attempt) = self.entries.get_mut(url) {
            attempt.pending = false;
            attempt.failures += 1;
            attempt.updated = now;
            attempt.retry_at = (retryable && attempt.failures <= 3)
                .then(|| now + Duration::from_secs(1 << (attempt.failures - 1)));
        }
    }

    pub fn succeeded(&mut self, url: &str) {
        self.entries.remove(url);
    }
    pub fn retry(&mut self, url: &str) {
        if self.entries.get(url).is_some_and(|a| !a.pending) {
            self.entries.remove(url);
        }
    }
    pub fn has_failed(&self, url: &str) -> bool {
        self.entries
            .get(url)
            .is_some_and(|a| !a.pending && a.failures > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failures_back_off_then_stop_until_manual_retry() {
        let mut loads = ImageLoads::default();
        let mut now = Instant::now();
        assert!(loads.request("image", now));
        assert!(!loads.request("image", now));
        for delay in [1, 2, 4] {
            loads.failed("image", true, now);
            assert!(!loads.request("image", now));
            now += Duration::from_secs(delay);
            assert!(loads.request("image", now));
        }
        loads.failed("image", true, now);
        assert!(!loads.request("image", now + Duration::from_secs(600)));
        loads.retry("image");
        assert!(loads.request("image", now));
        loads.succeeded("image");
        assert!(!loads.has_failed("image"));
        // A later cache eviction permits another download.
        assert!(loads.request("image", now));
    }
    #[test]
    fn permanent_failure_does_not_retry_and_pending_work_is_not_reset() {
        let mut loads = ImageLoads::default();
        let now = Instant::now();
        assert!(loads.request("image", now));
        loads.retry("image");
        assert!(!loads.request("image", now));
        loads.failed("image", false, now);
        assert!(!loads.request("image", now + Duration::from_secs(600)));
        loads.retry("image");
        assert!(loads.request("image", now));
    }
}
