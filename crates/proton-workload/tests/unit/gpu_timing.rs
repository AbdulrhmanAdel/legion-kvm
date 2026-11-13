use super::*;

fn calibration(gpu_ticks: u64, qpc_ticks: u64) -> GpuClockCalibration {
    GpuClockCalibration {
        gpu_ticks,
        qpc_ticks,
        call_begin_qpc: qpc_ticks - 1,
        call_end_qpc: qpc_ticks + 1,
    }
}

#[test]
fn different_clock_frequencies_and_large_origins_keep_microsecond_intervals() {
    let origin = u64::MAX - 10_000;
    let sample = GpuObservation {
        fence: 7,
        gpu_begin_ticks: origin + 2000,
        gpu_end_ticks: origin + 5000,
        submit_begin_qpc: 510,
        submit_end_qpc: 515,
        fence_verified_qpc: 590,
    };
    let report = summarize_gpu(
        &[sample],
        1_000_000_000,
        10_000_000,
        calibration(origin, 500),
        calibration(origin + 9000, 590),
    )
    .expect("valid clock domains");
    assert_eq!(report.execution.mean_us, 3.0);
    assert_eq!(
        report
            .submit_to_gpu_begin
            .expect("calibrated submission")
            .mean_us,
        1.0
    );
    assert_eq!(
        report
            .gpu_end_to_fence_verified
            .expect("calibrated completion")
            .mean_us,
        4.0
    );
    assert_eq!(report.calibration_drift_us, Some(0.0));
}

#[test]
fn calibration_errors_and_negative_estimates_remain_visible() {
    let sample = GpuObservation {
        fence: 1,
        gpu_begin_ticks: 110,
        gpu_end_ticks: 120,
        submit_begin_qpc: 115,
        submit_end_qpc: 116,
        fence_verified_qpc: 118,
    };
    let report = summarize_gpu(
        core::slice::from_ref(&sample),
        1_000_000,
        1_000_000,
        calibration(100, 100),
        calibration(130, 140),
    )
    .expect("valid clock domains");
    assert_eq!(report.negative_submit_intervals, 1);
    assert_eq!(report.negative_completion_intervals, 1);
    assert_eq!(
        report
            .gpu_end_to_fence_verified
            .expect("signed interval")
            .mean_us,
        -2.0
    );
    assert_eq!(report.calibration_drift_us, Some(10.0));
    let mut invalid = calibration(100, 100);
    invalid.qpc_ticks = 200;
    let report = summarize_gpu(
        &[sample],
        1_000_000,
        1_000_000,
        invalid,
        calibration(130, 140),
    )
    .expect("uncalibrated execution duration remains valid");
    assert_eq!(report.execution.mean_us, 10.0);
    assert!(report.gpu_end_to_fence_verified.is_none());
}

#[test]
fn uncompleted_queries_and_invalid_clocks_cannot_become_successful_timings() {
    let sample = GpuObservation {
        fence: 1,
        gpu_begin_ticks: 20,
        gpu_end_ticks: 10,
        submit_begin_qpc: 1,
        submit_end_qpc: 2,
        fence_verified_qpc: 3,
    };
    let summarize = |samples: &[GpuObservation], frequency| {
        summarize_gpu(
            samples,
            frequency,
            1_000_000,
            calibration(10, 10),
            calibration(30, 30),
        )
    };
    assert!(summarize(&[sample], 1_000_000).is_none());
    assert!(summarize(&[], 1_000_000).is_none());
    assert!(summarize(&[], 0).is_none());
}
