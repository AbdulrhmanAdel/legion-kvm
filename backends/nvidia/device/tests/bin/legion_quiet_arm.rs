use super::*;

#[test]
fn quiet_requests_require_negotiation_exact_size_and_zero_flags() {
    let good = reply(73, 0);
    assert_eq!(request(&good, true), Some(Ok(73)));
    assert_eq!(request(&good, false), Some(Err(())));
    assert_eq!(request(&good[..8], true), Some(Err(())));
    assert_eq!(request(&reply(73, -9), true), Some(Err(())));
    let mut extra = good.to_vec();
    extra.push(0);
    assert_eq!(request(&extra, true), Some(Err(())));
    let mut flags = good;
    flags[12] = 1;
    assert_eq!(request(&flags, true), Some(Err(())));
    assert_eq!(request(&[0; 16], true), None);
    let mut legacy = good;
    legacy[..4].copy_from_slice(&0x4c47_0011_u32.to_le_bytes());
    assert_eq!(request(&legacy, true), None);
}

#[test]
fn ordinary_or_error_responses_keep_the_interrupt_in_either_batch_order() {
    for order in [[true, false], [false, true]] {
        let mut batch = ControlBatch::default();
        for quiet in order {
            batch.completed(quiet);
        }
        assert!(batch.interrupt);
    }
    let mut batch = ControlBatch::default();
    assert!(!batch.interrupt);
    for _ in 0..1000 {
        batch.completed(true);
    }
    assert!(!batch.interrupt);
    batch.completed(false);
    assert!(batch.interrupt);
}
