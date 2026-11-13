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

impl PendingEvents {
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}
