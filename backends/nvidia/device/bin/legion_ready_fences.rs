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
#[path = "../tests/bin/legion_ready_fences.rs"]
mod tests;
