#![no_std]
//! Portable, deterministic input and correctness checks for the Windows probe.
extern crate alloc;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

mod gpu_timing;
pub use gpu_timing::{GpuClockCalibration, GpuObservation, GpuTiming, summarize_gpu};
mod presentation;
mod rendering;
pub use presentation::{
    FrameRate, PresentationConfig, frame_rate, frame_stage_ticks, presentation_pixel,
    verify_presentation, verify_presentation_load,
};
pub use rendering::{
    PipelineConfig, PipelineTiming, RenderImageCheck, shader_pixel, verify_shader,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    pub width: u32,
    pub height: u32,
    pub warmup_ms: u32,
    pub sample_ms: u32,
    pub cpu_rounds: u32,
    pub gpu_clears: u32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub diagnostic_completion_kick: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub gpu_timestamps: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub diagnostic_event_trace: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub diagnostic_cpu_profile: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<PipelineConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<PresentationConfig>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Config {
    pub fn valid(&self) -> bool {
        self.schema == 1
            && (64..=3840).contains(&self.width)
            && (64..=2160).contains(&self.height)
            && (100..=5000).contains(&self.warmup_ms)
            && (500..=10000).contains(&self.sample_ms)
            && (1..=1_000_000).contains(&self.cpu_rounds)
            && (1..=4096).contains(&self.gpu_clears)
            && (!self.diagnostic_event_trace || self.gpu_timestamps)
            && (!self.diagnostic_cpu_profile || self.pipeline.is_some())
            && self.presentation.as_ref().is_none_or(|p| {
                (15..=30).contains(&p.seconds)
                    && p.fragment_iterations <= 8192
                    && p.cpu_rounds <= 1_000_000
                    && self.pipeline.is_none()
                    && !self.diagnostic_cpu_profile
                    && !self.diagnostic_event_trace
                    && !self.diagnostic_completion_kick
            })
            && self.pipeline.as_ref().is_none_or(|p| {
                (1..=256).contains(&p.shader_rounds)
                    && (1..=64).contains(&p.draws_per_frame)
                    && (!p.retirement_probe || p.command_stage_probe)
                    && self.gpu_timestamps
                    && !self.diagnostic_completion_kick
            })
    }
}

/// Integer dependency chains with explicit wrapping, independent of CPUID/ISA.
#[inline(never)]
pub fn cpu_work(rounds: u32) -> u64 {
    let mut a = 0x1234_5678_9abc_def0_u64;
    let mut b = 0xfedc_ba98_7654_3210_u64;
    for i in 0..rounds {
        a = a.wrapping_add(b).rotate_left(17) ^ u64::from(i);
        b = b.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(a);
        core::hint::black_box((a, b));
    }
    a ^ b
}

/// Validate every texel while excluding D3D row alignment padding.
pub fn verify_red(bytes: &[u8], row_pitch: usize, width: usize, height: usize) -> Option<u64> {
    let row_size = width.checked_mul(4)?;
    if width == 0 || height == 0 || row_size > row_pitch {
        return None;
    }
    // D3D12 footprints need not include alignment padding after the last row.
    let required = (height - 1).checked_mul(row_pitch)?.checked_add(row_size)?;
    if bytes.len() < required {
        return None;
    }
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for row in 0..height {
        let start = row * row_pitch;
        for rgba in bytes[start..start + row_size].as_chunks::<4>().0 {
            if *rgba != [255, 0, 0, 255] {
                return None;
            }
            for &byte in rgba {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
            }
        }
    }
    Some(hash)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub mode: alloc::string::String,
    pub iterations: u64,
    pub elapsed_ms: f64,
    pub iterations_per_second: f64,
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub cpu_checksum: u64,
    #[serde(default)]
    pub stages: alloc::collections::BTreeMap<alloc::string::String, Timing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<PipelineTiming>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Timing {
    pub mean_us: f64,
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
}

/// Summarize QPC ticks after timing; reject empty samples or an invalid clock.
pub fn timing_summary(ticks: &mut [u64], frequency: u64) -> Option<Timing> {
    if ticks.is_empty() || frequency == 0 {
        return None;
    }
    ticks.sort_unstable();
    let us = |tick: u64| tick as f64 * 1_000_000.0 / frequency as f64;
    let sum: u128 = ticks.iter().map(|&tick| u128::from(tick)).sum();
    Some(Timing {
        mean_us: sum as f64 / ticks.len() as f64 * 1_000_000.0 / frequency as f64,
        p50_us: us(percentile(ticks, 50)),
        p95_us: us(percentile(ticks, 95)),
        p99_us: us(percentile(ticks, 99)),
    })
}

pub fn percentile(sorted: &[u64], numerator: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) * numerator / 100).min(sorted.len() - 1)]
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub schema: u32,
    pub passed: bool,
    pub gpu_verified: bool,
    pub workload: alloc::string::String,
    pub adapter: alloc::string::String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub logical_cpus: u32,
    pub xcr0: u64,
    #[serde(default)]
    pub qpc_frequency: u64,
    #[serde(default)]
    pub diagnostic_completion_kicks: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timestamp_validation_copy_ticks: Vec<u64>,
    pub config: Config,
    pub expected_cpu_checksum: u64,
    pub image_hash_before: u64,
    pub image_hash_after: u64,
    pub cases: Vec<Sample>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verification_ignores_padding_but_detects_stale_or_incorrect_pixels() {
        let mut image = [255, 0, 0, 255, 255, 0, 0, 255, 7, 7, 7, 7];
        assert!(verify_red(&image, 12, 2, 1).is_some());
        image[5] = 1;
        assert!(verify_red(&image, 12, 2, 1).is_none());
        assert!(verify_red(&image, 4, 2, 1).is_none());
        assert!(verify_red(&image, 12, 2, 2).is_none());
    }
    #[test]
    fn final_row_does_not_require_trailing_alignment_padding() {
        let image = [255, 0, 0, 255, 7, 7, 7, 7, 255, 0, 0, 255];
        let tight = [255, 0, 0, 255, 255, 0, 0, 255];
        assert_eq!(verify_red(&image, 8, 1, 2), verify_red(&tight, 4, 1, 2));
        assert!(verify_red(&image[..11], 8, 1, 2).is_none());
        assert!(verify_red(&image, 0, 0, 0).is_none());
    }
    #[test]
    fn cpu_result_depends_on_completed_work() {
        assert_eq!(cpu_work(10), cpu_work(10));
        assert_ne!(cpu_work(10), cpu_work(11));
    }
    #[test]
    fn stage_ticks_use_the_reported_clock_and_reject_missing_samples() {
        let summary = timing_summary(&mut [40, 10, 30, 20], 1_000_000).expect("valid clock");
        assert_eq!(summary.mean_us, 25.0);
        assert_eq!(summary.p50_us, 20.0);
        assert!(timing_summary(&mut [], 1_000_000).is_none());
        assert!(timing_summary(&mut [10], 0).is_none());
    }
    #[test]
    fn ordinary_configuration_preserves_compatibility_and_never_enables_queue_kicks() {
        let config: Config = serde_json::from_str(r#"{"schema":1,"width":1920,"height":1080,"warmup_ms":500,"sample_ms":2000,"cpu_rounds":100000,"gpu_clears":64}"#).expect("original configuration");
        assert!(config.valid());
        assert!(!config.diagnostic_completion_kick);
        assert!(!config.gpu_timestamps);
        assert!(!config.diagnostic_event_trace);
        let wire = serde_json::to_value(config).expect("serializable configuration");
        assert!(wire.get("diagnostic_completion_kick").is_none());
        assert!(wire.get("gpu_timestamps").is_none());
        assert!(wire.get("diagnostic_event_trace").is_none());
        assert!(wire.get("diagnostic_cpu_profile").is_none());
        assert!(wire.get("pipeline").is_none());
        assert!(wire.get("presentation").is_none());
    }
    #[test]
    fn presentation_load_preserves_defaults_and_bounds_both_work_dimensions() {
        let mut config: Config = serde_json::from_str(r#"{"schema":1,"width":1280,"height":720,"warmup_ms":200,"sample_ms":1000,"cpu_rounds":100000,"gpu_clears":64,"presentation":{"seconds":20}}"#).expect("original configuration");
        let wire = serde_json::to_value(&config).expect("serializable configuration");
        assert!(wire["presentation"].get("fragment_iterations").is_none());
        assert!(wire["presentation"].get("cpu_rounds").is_none());
        for (shader, cpu, valid) in [
            (0, 0, true),
            (8192, 1_000_000, true),
            (8193, 0, false),
            (0, 1_000_001, false),
            (u32::MAX, 0, false),
            (0, u32::MAX, false),
        ] {
            let settings = config.presentation.as_mut().expect("presentation");
            settings.fragment_iterations = shader;
            settings.cpu_rounds = cpu;
            assert_eq!(config.valid(), valid);
        }
    }

    #[test]
    fn windowed_fixture_is_bounded_and_separate_from_offscreen_diagnostics() {
        let base = r#"{"schema":1,"width":1280,"height":720,"warmup_ms":200,"sample_ms":1000,"cpu_rounds":100000,"gpu_clears":64,"presentation":{"seconds":20,"resize":true}}"#;
        let mut config: Config = serde_json::from_str(base).expect("windowed configuration");
        assert!(config.valid());
        for seconds in [0, 14, 31, u32::MAX] {
            config
                .presentation
                .as_mut()
                .expect("presentation configured")
                .seconds = seconds;
            assert!(!config.valid());
        }
        config
            .presentation
            .as_mut()
            .expect("presentation configured")
            .seconds = 20;
        config.diagnostic_completion_kick = true;
        assert!(!config.valid());
        config.diagnostic_completion_kick = false;
        config.diagnostic_cpu_profile = true;
        assert!(!config.valid());
        config.diagnostic_cpu_profile = false;
        config.pipeline = Some(PipelineConfig {
            shader_rounds: 1,
            draws_per_frame: 1,
            command_stage_probe: false,
            recording_probe: false,
            retirement_probe: false,
        });
        assert!(!config.valid());
    }
    #[test]
    fn pipeline_requires_real_timestamps_and_rejects_out_of_range_work_or_queue_kicks() {
        let mut config: Config = serde_json::from_str(r#"{"schema":1,"width":1920,"height":1080,"warmup_ms":100,"sample_ms":500,"cpu_rounds":100000,"gpu_clears":64,"gpu_timestamps":true,"pipeline":{"shader_rounds":32}}"#).expect("shader configuration");
        assert!(config.valid());
        config.gpu_timestamps = false;
        assert!(!config.valid());
        config.gpu_timestamps = true;
        config.diagnostic_completion_kick = true;
        assert!(!config.valid());
        config.diagnostic_completion_kick = false;
        for rounds in [0, 257] {
            config.pipeline = Some(PipelineConfig {
                shader_rounds: rounds,
                draws_per_frame: 1,
                command_stage_probe: false,
                recording_probe: false,
                retirement_probe: false,
            });
            assert!(!config.valid());
        }
    }

    #[test]
    fn retirement_control_requires_command_stage_timing() {
        let mut config: Config = serde_json::from_str(r#"{"schema":1,"width":1920,"height":1080,"warmup_ms":100,"sample_ms":500,"cpu_rounds":100000,"gpu_clears":64,"gpu_timestamps":true,"pipeline":{"shader_rounds":32,"retirement_probe":true}}"#).expect("diagnostic configuration");
        assert!(!config.valid());
        config
            .pipeline
            .as_mut()
            .expect("pipeline configured")
            .command_stage_probe = true;
        assert!(config.valid());
    }
}
