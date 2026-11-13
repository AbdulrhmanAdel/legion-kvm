//! Preserve consumable NVIDIA readiness across a full guest event virtqueue.
use std::collections::{HashSet, VecDeque};

#[derive(Default)]
pub struct PendingEvents {
    order: VecDeque<u32>,
    present: HashSet<u32>,
}

impl PendingEvents {
    pub fn mark(&mut self, handle: u32) {
        if self.present.insert(handle) {
            self.order.push_back(handle);
        }
    }

    pub fn remove(&mut self, handle: u32) {
        if self.present.remove(&handle) {
            self.order.retain(|&value| value != handle);
        }
    }

    /// Commit a notification only after the guest queue has accepted it.
    pub fn flush(&mut self, mut send: impl FnMut(u32) -> bool) -> usize {
        let mut sent = 0;
        while let Some(&handle) = self.order.front() {
            if !send(handle) {
                break;
            }
            self.order.pop_front();
            self.present.remove(&handle);
            sent += 1;
        }
        sent
    }
}

#[cfg(test)]
#[path = "../tests/bin/legion_event_retry.rs"]
mod tests;
