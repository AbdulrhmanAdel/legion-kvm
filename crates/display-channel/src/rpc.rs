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
#[path = "../tests/unit/rpc.rs"]
mod tests;
