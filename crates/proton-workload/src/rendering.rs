//! Deterministic integer pixel shader oracle and bounded pipeline measurements.
use crate::Timing;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineConfig {
    pub shader_rounds: u32,
    #[serde(default = "one")]
    pub draws_per_frame: u32,
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub command_stage_probe: bool,
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub recording_probe: bool,
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub retirement_probe: bool,
}

fn one() -> u32 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RenderImageCheck {
    pub seed: u32,
    pub hash: u64,
    pub pixels: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PipelineTiming {
    pub frames_in_flight: usize,
    pub maximum_outstanding: usize,
    pub completed_frames: u64,
    pub image_checks: Vec<RenderImageCheck>,
    /// Submission start to fence retirement; includes intentional queueing.
    pub retirement_latency: Timing,
    /// GPU timestamps between adjacent frames on this same queue.
    pub gpu_gap: Timing,
    /// Sum of measured execution / first begin to last end, excluding CPU clocks.
    pub gpu_active_fraction: f64,
}

/// Matches workload.hlsl exactly, including wrapping integer arithmetic.
pub fn shader_pixel(x: u32, y: u32, seed: u32, rounds: u32) -> [u8; 4] {
    let mut v = x.wrapping_add(y.wrapping_mul(65537)) ^ seed;
    for i in 0..rounds {
        v ^= v >> 16;
        v = v.wrapping_mul(0x7feb_352d);
        v ^= v >> 15;
        v = v.wrapping_mul(0x846c_a68b).wrapping_add(i);
    }
    [v as u8, (v >> 8) as u8, (v >> 16) as u8, 255]
}

/// Check all shader output bytes, excluding row padding.
pub fn verify_shader(
    bytes: &[u8],
    pitch: usize,
    width: u32,
    height: u32,
    seed: u32,
    rounds: u32,
) -> Option<RenderImageCheck> {
    let row = usize::try_from(width).ok()?.checked_mul(4)?;
    let rows = usize::try_from(height).ok()?;
    if row == 0
        || rows == 0
        || pitch < row
        || bytes.len() < (rows - 1).checked_mul(pitch)?.checked_add(row)?
    {
        return None;
    }
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for y in 0..height {
        for (x, rgba) in bytes[y as usize * pitch..y as usize * pitch + row]
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
        {
            if *rgba != shader_pixel(x as u32, y, seed, rounds) {
                return None;
            }
            for &v in rgba {
                hash = (hash ^ u64::from(v)).wrapping_mul(0x100_0000_01b3);
            }
        }
    }
    Some(RenderImageCheck {
        seed,
        hash,
        pixels: u64::from(width) * u64::from(height),
    })
}

#[cfg(test)]
#[path = "../tests/unit/rendering.rs"]
mod tests;
