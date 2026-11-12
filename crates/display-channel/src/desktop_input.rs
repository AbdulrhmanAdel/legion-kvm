use anyhow::{Result, ensure};

use crate::Input;

pub const BYTES: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesktopInput {
    pub input: Input,
    pub closed: bool,
    pub width: u32,
    pub height: u32,
}

impl DesktopInput {
    pub fn encode(self, out: &mut [u8]) -> Result<()> {
        ensure!(out.len() == BYTES, "invalid desktop input packet size");
        ensure!(
            (1..=3840).contains(&self.width) && (1..=2160).contains(&self.height),
            "invalid desktop input extent"
        );
        out.fill(0);
        out[..4].copy_from_slice(&1_u32.to_le_bytes());
        out[4..8].copy_from_slice(&self.input.generation().to_le_bytes());
        out[8..10].copy_from_slice(&self.input.x().to_le_bytes());
        out[10..12].copy_from_slice(&self.input.y().to_le_bytes());
        out[12..16].copy_from_slice(
            &(u32::from(self.input.space()) | u32::from(self.closed) << 1).to_le_bytes(),
        );
        out[16..20].copy_from_slice(&self.width.to_le_bytes());
        out[20..24].copy_from_slice(&self.height.to_le_bytes());
        Ok(())
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        ensure!(data.len() == BYTES, "invalid desktop input packet size");
        let word =
            |at| u32::from_le_bytes(data[at..at + 4].try_into().expect("fixed input packet"));
        ensure!(
            word(0) == 1 && word(12) & !3 == 0 && data[24..].iter().all(|b| *b == 0),
            "unknown desktop input version or flags"
        );
        let snapshot = Self {
            input: Input::new(
                u16::from_le_bytes([data[8], data[9]]),
                u16::from_le_bytes([data[10], data[11]]),
                word(12) & 1 != 0,
                word(4),
            )?,
            closed: word(12) & 2 != 0,
            width: word(16),
            height: word(20),
        };
        ensure!(
            (1..=3840).contains(&snapshot.width) && (1..=2160).contains(&snapshot.height),
            "invalid desktop input extent"
        );
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trip_and_rejection_boundaries() {
        let value = DesktopInput {
            input: Input::new(65535, 182, true, 0xff_ffff).expect("bounded input"),
            closed: true,
            width: 3840,
            height: 2160,
        };
        let mut wire = [0; BYTES];
        value.encode(&mut wire).expect("encode");
        assert_eq!(DesktopInput::decode(&wire).expect("decode"), value);
        for at in [0, 7, 15, 24, 31] {
            let mut invalid = wire;
            invalid[at] ^= 128;
            assert!(DesktopInput::decode(&invalid).is_err());
        }
        assert!(DesktopInput::decode(&wire[..31]).is_err());
        let mut invalid = wire;
        invalid[16..20].copy_from_slice(&0_u32.to_le_bytes());
        assert!(DesktopInput::decode(&invalid).is_err());
        assert!(
            DesktopInput { width: 0, ..value }
                .encode(&mut wire)
                .is_err()
        );
    }
}
