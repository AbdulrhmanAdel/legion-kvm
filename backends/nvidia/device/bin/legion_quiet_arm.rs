//! Negotiated quiet wait-arm acknowledgements, not GPU completions.

pub const FEATURE: u64 = 1;
pub const MESSAGE: u32 = 0x4c47_0012;

pub fn request(bytes: &[u8], negotiated: bool) -> Option<Result<u32, ()>> {
    if bytes.get(..4) != Some(MESSAGE.to_le_bytes().as_slice()) {
        return None;
    }
    if !negotiated || bytes.len() != 16 || bytes[8..].iter().any(|&b| b != 0) {
        return Some(Err(()));
    }
    Some(Ok(u32::from_le_bytes(
        bytes[4..8].try_into().expect("checked header"),
    )))
}

pub fn reply(handle: u32, status: i32) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..4].copy_from_slice(&MESSAGE.to_le_bytes());
    bytes[4..8].copy_from_slice(&handle.to_le_bytes());
    bytes[8..12].copy_from_slice(&status.to_le_bytes());
    bytes
}

#[derive(Default)]
pub struct ControlBatch {
    pub interrupt: bool,
}

impl ControlBatch {
    /// A quiet acknowledgement may share a batch with ordinary responses.
    pub fn completed(&mut self, acknowledged_quiet: bool) {
        self.interrupt |= !acknowledged_quiet;
    }
}

#[cfg(test)]
#[path = "../tests/bin/legion_quiet_arm.rs"]
mod tests;
