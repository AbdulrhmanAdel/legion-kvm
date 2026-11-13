//! Private subscription messages and bounded per-open poll interest.
use std::collections::HashSet;

pub const ARM: u32 = 0x4c47_0011;

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
#[path = "../tests/bin/legion_poll_interest.rs"]
mod tests;
