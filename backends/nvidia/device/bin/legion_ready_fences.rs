//! Pending sync-file readiness is level-triggered and must survive backpressure.
use std::collections::BTreeSet;

#[derive(Default)]
pub struct ReadyFences {
    ready: BTreeSet<u64>,
}

impl ReadyFences {
    pub fn mark(&mut self, key: u64) {
        self.ready.insert(key);
    }

    pub fn contains(&self, key: u64) -> bool {
        self.ready.contains(&key)
    }

    pub fn remove(&mut self, key: u64) {
        self.ready.remove(&key);
    }

    /// Retire only accepted deliveries (or confirmed stale watches).
    pub fn retry(&mut self, mut accepted: impl FnMut(u64) -> bool) {
        self.ready.retain(|key| !accepted(*key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backpressure_preserves_distinct_64_bit_keys_until_acceptance() {
        let a = (1_u64 << 63) | 1;
        let b = (1_u64 << 63) | (1_u64 << 32) | 1;
        let mut pending = ReadyFences::default();
        pending.mark(a);
        pending.mark(a);
        pending.mark(b);
        let mut seen = Vec::new();
        pending.retry(|key| {
            seen.push(key);
            false
        });
        assert_eq!(seen, vec![a, b]);
        assert!(pending.contains(a) && pending.contains(b));
        pending.retry(|key| key == a);
        assert!(!pending.contains(a) && pending.contains(b));
        pending.retry(|_| true);
        assert!(!pending.contains(b));
    }

    #[test]
    fn cancellation_cannot_redeliver_a_pending_watch() {
        let key = (1_u64 << 63) | 9;
        let mut pending = ReadyFences::default();
        pending.mark(key);
        pending.remove(key);
        pending.retry(|_| panic!("canceled watch delivered"));
        pending.mark(key + 1);
        let mut calls = 0;
        pending.retry(|_| {
            calls += 1;
            true
        });
        pending.retry(|_| {
            calls += 1;
            true
        });
        assert_eq!(calls, 1);
    }
}
