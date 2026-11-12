//! Pointer-free display RPC packets shared by guest and host adapters.
#![forbid(unsafe_code)]

use anyhow::{Result, ensure};

pub const FRAME_BYTES: usize = 16;
pub const RELEASE_CAPACITY: usize = 16;
pub const RELEASE_BYTES: usize = 8 + RELEASE_CAPACITY * FRAME_BYTES;
pub const EXCHANGE_BYTES: usize = FRAME_BYTES + RELEASE_BYTES;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub resource: u64,
    pub frame: u64,
}

impl Frame {
    pub fn encode(self) -> Result<[u8; FRAME_BYTES]> {
        ensure!(
            self.resource > 0 && self.frame > 0,
            "invalid frame identity"
        );
        let mut out = [0; FRAME_BYTES];
        out[..8].copy_from_slice(&self.resource.to_le_bytes());
        out[8..].copy_from_slice(&self.frame.to_le_bytes());
        Ok(out)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        ensure!(data.len() == FRAME_BYTES, "frame packet size mismatch");
        let value = Self {
            resource: u64::from_le_bytes(data[..8].try_into()?),
            frame: u64::from_le_bytes(data[8..].try_into()?),
        };
        ensure!(
            value.resource > 0 && value.frame > 0,
            "invalid frame identity"
        );
        Ok(value)
    }
}

#[derive(Default)]
pub struct Releases {
    entries: [Frame; RELEASE_CAPACITY],
    count: usize,
}

impl Releases {
    pub fn as_slice(&self) -> &[Frame] {
        &self.entries[..self.count]
    }

    pub fn push(&mut self, value: Frame) -> Result<()> {
        ensure!(self.count < RELEASE_CAPACITY, "release packet full");
        ensure!(
            value.resource > 0 && value.frame > 0,
            "invalid release identity"
        );
        ensure!(
            !self.as_slice().iter().any(|f| f.resource == value.resource),
            "duplicate release resource"
        );
        self.entries[self.count] = value;
        self.count += 1;
        Ok(())
    }

    pub fn encode(&self, out: &mut [u8]) -> Result<()> {
        ensure!(out.len() == RELEASE_BYTES, "release packet size mismatch");
        out.fill(0);
        out[..4].copy_from_slice(&u32::try_from(self.count)?.to_le_bytes());
        for (index, frame) in self.as_slice().iter().enumerate() {
            let at = 8 + index * FRAME_BYTES;
            out[at..at + FRAME_BYTES].copy_from_slice(&frame.encode()?);
        }
        Ok(())
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        ensure!(data.len() == RELEASE_BYTES, "release packet size mismatch");
        let count = usize::try_from(u32::from_le_bytes(data[..4].try_into()?))?;
        ensure!(
            count <= RELEASE_CAPACITY && data[4..8] == [0; 4],
            "invalid release header"
        );
        let mut result = Self::default();
        for index in 0..count {
            let at = 8 + index * FRAME_BYTES;
            result.push(Frame::decode(&data[at..at + FRAME_BYTES])?)?;
        }
        Ok(result)
    }

    /// Check every acknowledgement before the caller mutates any live leases.
    pub fn validate(&self, active_frame: impl Fn(u64) -> Option<u64>) -> Result<()> {
        for release in self.as_slice() {
            ensure!(
                active_frame(release.resource) == Some(release.frame),
                "release generation mismatch or unknown resource"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_preserves_submission_and_can_release_that_same_frame() {
        let frame = Frame {
            resource: 7,
            frame: 19,
        };
        let mut packet = [0; EXCHANGE_BYTES];
        packet[..FRAME_BYTES].copy_from_slice(&frame.encode().expect("frame"));
        let mut releases = Releases::default();
        releases.push(frame).expect("release");
        releases.encode(&mut packet[FRAME_BYTES..]).expect("encode");
        assert_eq!(
            Frame::decode(&packet[..FRAME_BYTES]).expect("decode"),
            frame
        );
        let reply = Releases::decode(&packet[FRAME_BYTES..]).expect("decode");
        reply
            .validate(|id| (id == 7).then_some(19))
            .expect("new submission already leased");
        assert!(reply.validate(|_| None).is_err());
    }

    #[test]
    fn malformed_size_count_padding_and_duplicate_resources_are_rejected() {
        assert!(Releases::decode(&[0; RELEASE_BYTES - 1]).is_err());
        let mut packet = [0; RELEASE_BYTES];
        packet[..4].copy_from_slice(&17_u32.to_le_bytes());
        assert!(Releases::decode(&packet).is_err());
        packet[..4].copy_from_slice(&0_u32.to_le_bytes());
        packet[4] = 1;
        assert!(Releases::decode(&packet).is_err());
        packet[4] = 0;
        packet[..4].copy_from_slice(&2_u32.to_le_bytes());
        let frame = Frame {
            resource: 7,
            frame: 1,
        }
        .encode()
        .expect("frame");
        packet[8..24].copy_from_slice(&frame);
        packet[24..40].copy_from_slice(&frame);
        assert!(Releases::decode(&packet).is_err());
        assert!(Frame::decode(&[0; FRAME_BYTES]).is_err());
    }

    #[test]
    fn mixed_stale_and_unknown_acknowledgements_do_not_change_active_leases() {
        let active = [(7, 3), (8, 5)];
        let lookup = |id| {
            active
                .iter()
                .find(|(resource, _)| *resource == id)
                .map(|(_, frame)| *frame)
        };
        for bad in [
            Frame {
                resource: 8,
                frame: 4,
            },
            Frame {
                resource: 9,
                frame: 5,
            },
        ] {
            let mut releases = Releases::default();
            releases
                .push(Frame {
                    resource: 7,
                    frame: 3,
                })
                .expect("valid first release");
            releases.push(bad).expect("second release");
            assert!(releases.validate(lookup).is_err());
            assert_eq!(active, [(7, 3), (8, 5)]);
        }
    }

    #[test]
    fn release_capacity_is_bounded_and_unused_reply_bytes_are_cleared() {
        let mut releases = Releases::default();
        for id in 1..=16 {
            releases
                .push(Frame {
                    resource: id,
                    frame: id + 1,
                })
                .expect("capacity");
        }
        assert!(
            releases
                .push(Frame {
                    resource: 17,
                    frame: 18
                })
                .is_err()
        );
        let mut packet = [255; RELEASE_BYTES];
        releases.encode(&mut packet).expect("encode full");
        assert_eq!(
            Releases::decode(&packet).expect("decode").as_slice().len(),
            16
        );
        Releases::default()
            .encode(&mut packet)
            .expect("encode empty");
        assert_eq!(packet, [0; RELEASE_BYTES]);
    }
}
