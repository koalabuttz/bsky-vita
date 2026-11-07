use std::time::{Duration, Instant};

/// Frame-driven retry deadlines; never sleeps on the render or network thread.
pub(crate) struct ReadRetry {
    failures: u32,
    due: Option<Instant>,
}
impl ReadRetry {
    pub fn new(now: Instant) -> Self {
        Self {
            failures: 0,
            due: Some(now),
        }
    }
    pub fn ready(&self, now: Instant) -> bool {
        self.due.is_some_and(|at| now >= at)
    }
    pub fn failed(&mut self, retryable: bool, now: Instant) {
        self.failures += 1;
        self.due = (retryable && self.failures <= 3)
            .then(|| now + Duration::from_secs(1 << (self.failures - 1)));
    }
    pub fn scheduled(&self) -> bool {
        self.due.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_deadlines_are_bounded_and_permanent_errors_stop() {
        let mut now = Instant::now();
        let mut retry = ReadRetry::new(now);
        assert!(retry.ready(now));
        for delay in [1, 2, 4] {
            retry.failed(true, now);
            assert!(!retry.ready(now));
            now += Duration::from_secs(delay);
            assert!(retry.ready(now));
        }
        retry.failed(true, now);
        assert!(!retry.ready(now + Duration::from_secs(1000)));
        let mut retry = ReadRetry::new(now);
        retry.failed(false, now);
        assert!(!retry.scheduled());
    }
}
