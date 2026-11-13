use super::*;
#[test]
fn opening_does_not_subscribe_and_reuse_cannot_inherit_interest() {
    let mut state = Interests::default();
    state.opened(5);
    assert!(!state.contains(5));
    assert!(state.subscribe(5));
    assert!(!state.subscribe(5));
    state.close(5);
    assert!(!state.subscribe(5));
    state.opened(5);
    assert!(!state.contains(5));
    assert!(state.subscribe(5));
}
#[test]
fn unknown_handles_do_not_allocate_subscription_state() {
    let mut state = Interests::default();
    for handle in 0..10000 {
        assert!(!state.subscribe(handle));
    }
    assert!(state.interested.is_empty());
    assert!(state.live.is_empty());
}
#[test]
fn one_shot_readiness_requires_a_new_wait_before_polling_again() {
    let mut state = Interests::default();
    state.opened(8);
    assert!(state.subscribe(8));
    state.disarm(8);
    assert!(!state.contains(8));
    assert!(state.subscribe(8));
    assert!(!state.subscribe(8));
    state.close(8);
    assert!(!state.subscribe(8));
}
#[test]
fn private_requests_reject_truncation_payloads_and_nonzero_flags() {
    let good = reply(17, 0);
    assert_eq!(request(&good), Some(Ok(17)));
    assert_eq!(request(&good[..8]), Some(Err(())));
    assert_eq!(request(&reply(17, 1)), Some(Err(())));
    let mut oversized = good.to_vec();
    oversized.push(0);
    assert_eq!(request(&oversized), Some(Err(())));
    assert_eq!(request(&[0; 16]), None);
    assert_eq!(arm_request(&good), None);
    assert_eq!(request(&arm_reply(17, 0)), None);
    assert_eq!(arm_request(&arm_reply(17, 0)), Some(Ok(17)));
}

pub const SUBSCRIBE: u32 = 0x4c47_0010;
/// None leaves ordinary protocol dispatch untouched.
pub fn request(bytes: &[u8]) -> Option<Result<u32, ()>> {
    decode(bytes, SUBSCRIBE)
}
pub fn reply(handle: u32, status: i32) -> [u8; 16] {
    encode(SUBSCRIBE, handle, status)
}
