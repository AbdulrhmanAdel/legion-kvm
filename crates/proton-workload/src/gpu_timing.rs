//! GPU-clock durations and explicitly calibrated CPU/GPU observations.
use crate::{Timing, timing_summary};
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

/// One D3D12 GPU/QPC clock pair, bracketed by calls to QPC.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GpuClockCalibration {
    pub gpu_ticks: u64,
    pub qpc_ticks: u64,
    pub call_begin_qpc: u64,
    pub call_end_qpc: u64,
}

impl GpuClockCalibration {
    fn valid(&self) -> bool {
        self.call_begin_qpc <= self.qpc_ticks && self.qpc_ticks <= self.call_end_qpc
    }
}

/// Real query results read only after the submission's original fence completes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GpuObservation {
    pub fence: u64,
    pub gpu_begin_ticks: u64,
    pub gpu_end_ticks: u64,
    pub submit_begin_qpc: u64,
    pub submit_end_qpc: u64,
    pub fence_verified_qpc: u64,
}

/// Instrumented timings; calibration-derived intervals retain negative values.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GpuTiming {
    pub timestamp_frequency: u64,
    pub sample_count: usize,
    pub execution: Timing,
    pub clock_before: GpuClockCalibration,
    pub clock_after: GpuClockCalibration,
    pub calibration_drift_us: Option<f64>,
    pub submit_to_gpu_begin: Option<Timing>,
    pub gpu_end_to_fence_verified: Option<Timing>,
    pub negative_submit_intervals: usize,
    pub negative_completion_intervals: usize,
    /// Bounded first observations for joining separate diagnostic traces.
    pub trace: Vec<GpuObservation>,
}

fn signed_summary(values: &mut [f64]) -> Option<Timing> {
    if values.is_empty() || values.iter().any(|v| !v.is_finite()) {
        return None;
    }
    values.sort_unstable_by(f64::total_cmp);
    let at = |p: usize| values[(values.len() - 1) * p / 100];
    Some(Timing {
        mean_us: values.iter().sum::<f64>() / values.len() as f64,
        p50_us: at(50),
        p95_us: at(95),
        p99_us: at(99),
    })
}

/// Summarize actual GPU execution and estimates relative to the initial clock pair.
pub fn summarize_gpu(
    samples: &[GpuObservation],
    gpu_frequency: u64,
    qpc_frequency: u64,
    before: GpuClockCalibration,
    after: GpuClockCalibration,
) -> Option<GpuTiming> {
    if samples.is_empty() || gpu_frequency == 0 || qpc_frequency == 0 {
        return None;
    }
    let mut ticks = samples
        .iter()
        .map(|sample| sample.gpu_end_ticks.checked_sub(sample.gpu_begin_ticks))
        .collect::<Option<Vec<_>>>()?;
    if samples
        .iter()
        .any(|s| s.submit_end_qpc < s.submit_begin_qpc || s.fence_verified_qpc < s.submit_end_qpc)
    {
        return None;
    }
    let execution = timing_summary(&mut ticks, gpu_frequency)?;
    // Subtract in signed integer space before converting, avoiding cancellation of large absolute counters and preserving observations before calibration.
    let cpu_delta_us =
        |a, b| (i128::from(a) - i128::from(b)) as f64 * 1_000_000.0 / qpc_frequency as f64;
    let gpu_delta_us =
        |a, b| (i128::from(a) - i128::from(b)) as f64 * 1_000_000.0 / gpu_frequency as f64;
    let calibrated = before.valid()
        && after.valid()
        && after.gpu_ticks >= before.gpu_ticks
        && after.qpc_ticks >= before.qpc_ticks;
    let mut submit = Vec::new();
    let mut completion = Vec::new();
    let mut negative_submit_intervals = 0;
    let mut negative_completion_intervals = 0;
    if calibrated {
        submit.reserve(samples.len());
        completion.reserve(samples.len());
        for sample in samples {
            let to_begin = gpu_delta_us(sample.gpu_begin_ticks, before.gpu_ticks)
                - cpu_delta_us(sample.submit_begin_qpc, before.qpc_ticks);
            let from_end = cpu_delta_us(sample.fence_verified_qpc, before.qpc_ticks)
                - gpu_delta_us(sample.gpu_end_ticks, before.gpu_ticks);
            negative_submit_intervals += usize::from(to_begin < 0.0);
            negative_completion_intervals += usize::from(from_end < 0.0);
            submit.push(to_begin);
            completion.push(from_end);
        }
    }
    Some(GpuTiming {
        timestamp_frequency: gpu_frequency,
        sample_count: samples.len(),
        execution,
        calibration_drift_us: calibrated.then(|| {
            cpu_delta_us(after.qpc_ticks, before.qpc_ticks)
                - gpu_delta_us(after.gpu_ticks, before.gpu_ticks)
        }),
        clock_before: before,
        clock_after: after,
        submit_to_gpu_begin: signed_summary(&mut submit),
        gpu_end_to_fence_verified: signed_summary(&mut completion),
        negative_submit_intervals,
        negative_completion_intervals,
        trace: samples.iter().take(1024).cloned().collect(),
    })
}

#[cfg(test)]
#[path = "../tests/unit/gpu_timing.rs"]
mod tests;
