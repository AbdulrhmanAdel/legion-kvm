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
mod tests {
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
}
