use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationConfig {
    pub seconds: u32,
    #[serde(default)]
    pub resize: bool,
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub stage_probe: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fragment_iterations: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cpu_rounds: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

// Affine exponentiation computes the shader's wrapping recurrence without repeating thousands of GPU iterations for every CPU reference pixel.
fn load_transform(mut rounds: u32) -> (u32, u32) {
    let (mut a, mut c) = (1_u32, 0_u32);
    let (mut step_a, mut step_c) = (1_664_525_u32, 1_013_904_223_u32);
    while rounds != 0 {
        if rounds & 1 != 0 {
            c = c.wrapping_mul(step_a).wrapping_add(step_c);
            a = a.wrapping_mul(step_a);
        }
        step_c = step_c.wrapping_mul(step_a.wrapping_add(1));
        step_a = step_a.wrapping_mul(step_a);
        rounds >>= 1;
    }
    (a, c)
}

fn load_seed(x: u32, y: u32, width: u32, seed: u32, transform: (u32, u32)) -> u32 {
    let state = (seed ^ x.wrapping_add(y.wrapping_mul(width)))
        .wrapping_mul(transform.0)
        .wrapping_add(transform.1);
    state ^ (state >> 16)
}

#[derive(Debug, Serialize)]
pub struct FrameRate {
    pub average_fps: f64,
    /// Reciprocal of the mean duration of the slowest ceil(1%) of frames.
    pub one_percent_low_fps: f64,
}

/// Source render rate, including stalls.
pub fn frame_rate(sorted: &[u64], frequency: u64) -> Option<FrameRate> {
    if sorted.is_empty() || frequency == 0 || !sorted.is_sorted() {
        return None;
    }
    let total: u128 = sorted.iter().map(|&n| u128::from(n)).sum();
    let count = sorted.len().div_ceil(100);
    let tail: u128 = sorted[sorted.len() - count..]
        .iter()
        .map(|&n| u128::from(n))
        .sum();
    if total == 0 || tail == 0 {
        return None;
    }
    Some(FrameRate {
        average_fps: sorted.len() as f64 * frequency as f64 / total as f64,
        one_percent_low_fps: count as f64 * frequency as f64 / tail as f64,
    })
}

/// Disjoint source-frame stages.
pub fn frame_stage_ticks(boundaries: [u64; 9]) -> Option<[u64; 8]> {
    let mut stages = [0; 8];
    for (stage, pair) in stages.iter_mut().zip(boundaries.windows(2)) {
        *stage = pair[1].checked_sub(pair[0])?;
    }
    Some(stages)
}

/// Exact integer output of the two owned presentation pixel shaders.
pub fn presentation_pixel(x: u32, y: u32, width: u32, seed: u32) -> [u8; 4] {
    let r = x.wrapping_add(seed) as u8;
    let g = y.wrapping_add(seed >> 1) as u8;
    let b = if ((x / 64) ^ (y / 64) ^ (seed >> 7)) & 1 != 0 {
        192
    } else {
        32
    };
    if x >= width / 2 {
        [b, g, r, 255]
    } else {
        [r, g, b, 255]
    }
}

/// Validate every RGBA8 pixel, excluding D3D12 row-alignment padding.
pub fn verify_presentation(
    bytes: &[u8],
    pitch: usize,
    width: u32,
    height: u32,
    seed: u32,
) -> Option<u64> {
    verify_presentation_load(bytes, pitch, width, height, seed, 0)
}

/// Verify the optional shader load against an independent closed-form oracle.
pub fn verify_presentation_load(
    bytes: &[u8],
    pitch: usize,
    width: u32,
    height: u32,
    seed: u32,
    iterations: u32,
) -> Option<u64> {
    let row = usize::try_from(width).ok()?.checked_mul(4)?;
    let rows = usize::try_from(height).ok()?;
    if row == 0
        || rows == 0
        || pitch < row
        || bytes.len() < pitch.checked_mul(rows - 1)?.checked_add(row)?
    {
        return None;
    }
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let transform = load_transform(iterations);
    for y in 0..height {
        for (x, pixel) in bytes[y as usize * pitch..y as usize * pitch + row]
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
        {
            let mut expected = presentation_pixel(x as u32, y, width, seed);
            if iterations != 0 {
                let channel = if x as u32 >= width / 2 { 0 } else { 2 };
                expected[channel] += (load_seed(x as u32, y, width, seed, transform) & 1) as u8;
            }
            if *pixel != expected {
                return None;
            }
            for b in pixel {
                hash = (hash ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3);
            }
        }
    }
    Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closed_form_load_matches_naive_wrapping_iterations_and_high_bits() {
        for rounds in [1, 2, 3, 31, 512, 8192] {
            let transform = load_transform(rounds);
            for seed in [0, u32::MAX, 73] {
                for (x, y, width) in [(0, 0, 64), (63, 17, 64), (1919, 1079, 1920)] {
                    let mut state = seed ^ (x + y * width);
                    for _ in 0..rounds {
                        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    }
                    assert_eq!(
                        load_seed(x, y, width, seed, transform),
                        state ^ (state >> 16)
                    );
                }
            }
        }
        assert_ne!(
            load_seed(1, 1, 64, 73, load_transform(8192)),
            load_seed(1, 1, 64, 73, load_transform(512))
        );
    }

    #[test]
    fn loaded_image_oracle_rejects_wrong_cost_and_any_corrupted_pixel() {
        let mut data = [0_u8; 32];
        for y in 0..2_u32 {
            for x in 0..4_u32 {
                let mut seed = 73 ^ (x + y * 4);
                for _ in 0..31 {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                }
                let mut pixel = presentation_pixel(x, y, 4, 73);
                pixel[if x >= 2 { 0 } else { 2 }] += ((seed ^ (seed >> 16)) & 1) as u8;
                let offset = (y * 16 + x * 4) as usize;
                data[offset..offset + 4].copy_from_slice(&pixel);
            }
        }
        assert!(verify_presentation_load(&data, 16, 4, 2, 73, 31).is_some());
        assert!(verify_presentation_load(&data, 16, 4, 2, 73, 30).is_none());
        data[19] ^= 1;
        assert!(verify_presentation_load(&data, 16, 4, 2, 73, 31).is_none());
    }
    #[test]
    fn fps_includes_stalls_and_uses_the_slowest_one_percent_mean() {
        let mut ticks = [10_u64; 101];
        ticks[99] = 20;
        ticks[100] = 180;
        let rate = frame_rate(&ticks, 1000).expect("valid frame times");
        assert_eq!(rate.average_fps, 101000.0 / 1190.0);
        assert_eq!(rate.one_percent_low_fps, 10.0);
        assert!(frame_rate(&[], 1000).is_none());
        assert!(frame_rate(&[0, 0], 1000).is_none());
        assert!(frame_rate(&[10], 0).is_none());
        assert!(frame_rate(&[20, 10], 1000).is_none());
        let stages = frame_stage_ticks([0, 2, 3, 5, 8, 13, 21, 34, 55]).expect("monotonic stamps");
        assert_eq!(stages.iter().sum::<u64>(), 55);
        assert!(frame_stage_ticks([0, 2, 3, 5, 4, 13, 21, 34, 55]).is_none());
    }
    #[test]
    fn oracle_checks_both_pipeline_outputs_seed_and_alignment_padding() {
        let mut data = alloc::vec![7;40];
        for y in 0..2 {
            for x in 0..4 {
                let at = y * 24 + x * 4;
                data[at..at + 4].copy_from_slice(&presentation_pixel(x as u32, y as u32, 4, 73));
            }
        }
        assert!(verify_presentation(&data, 24, 4, 2, 73).is_some());
        assert!(verify_presentation(&data, 24, 4, 2, 74).is_none());
        data[32] ^= 1;
        assert!(verify_presentation(&data, 24, 4, 2, 73).is_none());
        assert!(verify_presentation(&data[..39], 24, 4, 2, 73).is_none());
    }
}
