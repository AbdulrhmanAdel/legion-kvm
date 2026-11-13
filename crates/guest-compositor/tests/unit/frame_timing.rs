use super::*;
#[test]
fn cadence_keeps_a_hitch_crossing_a_report_boundary_and_resets_on_window_switch() {
    let start = Instant::now();
    let mut cadence = Cadence::default();
    cadence.record(start);
    cadence.record(start + Duration::from_millis(8));
    assert_eq!(cadence.take()["max_ms"], 8.0);
    cadence.record(start + Duration::from_millis(108));
    let report = cadence.take();
    assert_eq!(report["max_ms"], 100.0);
    assert_eq!(report["over_50_ms"], 1);
    cadence.reset_clock();
    cadence.record(start + Duration::from_secs(20));
    assert_eq!(cadence.take()["count"], 0);
}
#[test]
fn overflow_retains_total_and_max_without_publishing_biased_percentiles() {
    let mut samples = Distribution::default();
    for _ in 0..CAPACITY {
        samples.record(Duration::from_millis(1));
    }
    samples.record(Duration::from_millis(100));
    let report = samples.take();
    assert_eq!(report["dropped_samples"], 1);
    assert_eq!(report["max_ms"], 100.0);
    assert!(report["p99_ms"].is_null());
    let empty = samples.take();
    assert!(empty["mean_ms"].is_null());
    assert!(empty["p50_ms"].is_null());
}
