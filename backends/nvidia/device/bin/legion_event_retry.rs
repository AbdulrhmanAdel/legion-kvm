//! Preserve consumable NVIDIA readiness across a full guest event virtqueue.
use std::collections::{HashSet, VecDeque};

#[derive(Default)]
pub struct PendingEvents {
    order: VecDeque<u32>,
    present: HashSet<u32>,
}

impl PendingEvents {
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
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
mod tests {
    use super::*;
    #[test]
    fn full_queue_preserves_consumed_readiness_for_retry() {
        let mut pending = PendingEvents::default();
        pending.mark(17);
        assert_eq!(pending.flush(|_| false), 0);
        let mut received = Vec::new();
        assert_eq!(
            pending.flush(|value| {
                received.push(value);
                true
            }),
            1
        );
        assert_eq!(received, [17]);
        assert_eq!(pending.flush(|_| panic!("already delivered")), 0);
    }
    #[test]
    fn duplicates_coalesce_and_ready_handles_remain_fair() {
        let mut pending = PendingEvents::default();
        for handle in [1, 2, 1] {
            pending.mark(handle);
        }
        let mut received = Vec::new();
        pending.flush(|value| {
            if !received.is_empty() {
                return false;
            }
            received.push(value);
            true
        });
        pending.mark(1);
        pending.flush(|value| {
            received.push(value);
            true
        });
        assert_eq!(received, [1, 2, 1]);
    }
    #[test]
    fn closing_a_handle_removes_its_pending_notification_before_reuse() {
        let mut pending = PendingEvents::default();
        pending.mark(7);
        pending.mark(8);
        pending.remove(7);
        pending.mark(7);
        let mut received = Vec::new();
        pending.flush(|value| {
            received.push(value);
            true
        });
        assert_eq!(received, [8, 7]);
    }
    #[test]
    fn burst_larger_than_guest_buffers_loses_no_handle() {
        let mut pending = PendingEvents::default();
        for handle in 0..24 {
            pending.mark(handle);
        }
        let mut received = Vec::new();
        assert_eq!(
            pending.flush(|value| {
                if received.len() == 16 {
                    return false;
                }
                received.push(value);
                true
            }),
            16
        );
        assert_eq!(
            pending.flush(|value| {
                received.push(value);
                true
            }),
            8
        );
        assert_eq!(received, (0..24).collect::<Vec<_>>());
    }
}
