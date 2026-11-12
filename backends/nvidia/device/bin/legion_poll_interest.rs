//! Private subscription messages and bounded per-open poll interest.
use std::collections::HashSet;

#[cfg(test)]
pub const SUBSCRIBE: u32 = 0x4c47_0010;
pub const ARM: u32 = 0x4c47_0011;

/// None leaves ordinary protocol dispatch untouched.
#[cfg(test)]
pub fn request(bytes: &[u8]) -> Option<Result<u32, ()>> {
    decode(bytes, SUBSCRIBE)
}

pub fn arm_request(bytes: &[u8]) -> Option<Result<u32, ()>> {
    decode(bytes, ARM)
}

fn decode(bytes: &[u8], message: u32) -> Option<Result<u32, ()>> {
    if bytes.get(..4) != Some(message.to_le_bytes().as_slice()) {
        return None;
    }
    if bytes.len() != 16 || bytes[8..].iter().any(|&b| b != 0) {
        return Some(Err(()));
    }
    Some(Ok(u32::from_le_bytes(
        bytes[4..8].try_into().expect("checked header"),
    )))
}

#[cfg(test)]
pub fn reply(handle: u32, status: i32) -> [u8; 16] {
    encode(SUBSCRIBE, handle, status)
}
pub fn arm_reply(handle: u32, status: i32) -> [u8; 16] {
    encode(ARM, handle, status)
}
fn encode(message: u32, handle: u32, status: i32) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..4].copy_from_slice(&message.to_le_bytes());
    bytes[4..8].copy_from_slice(&handle.to_le_bytes());
    bytes[8..12].copy_from_slice(&status.to_le_bytes());
    bytes
}

#[derive(Default)]
pub struct Interests {
    live: HashSet<u32>,
    interested: HashSet<u32>,
}
impl Interests {
    pub fn opened(&mut self, handle: u32) {
        self.live.insert(handle);
        self.interested.remove(&handle);
    }
    /// True only for the first subscription on a live open file.
    pub fn subscribe(&mut self, handle: u32) -> bool {
        self.live.contains(&handle) && self.interested.insert(handle)
    }
    pub fn contains(&self, handle: u32) -> bool {
        self.interested.contains(&handle)
    }
    pub fn disarm(&mut self, handle: u32) {
        self.interested.remove(&handle);
    }
    pub fn close(&mut self, handle: u32) {
        self.live.remove(&handle);
        self.interested.remove(&handle);
    }
}

#[cfg(test)]
mod tests {
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
}
