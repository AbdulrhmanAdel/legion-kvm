use super::*;

#[test]
fn tight_gpu_polling_does_not_repeat_filesystem_checks_and_exit_is_latched() {
    let directory = tempfile::tempdir().expect("private marker directory");
    let path = directory.path().join("client-exited");
    let start = Instant::now();
    let mut marker = ExitMarker::new(&path, start);
    assert!(!marker.exited(start));
    std::fs::write(&path, []).expect("application exits");
    for microseconds in 1..10_000 {
        assert!(!marker.exited(start + Duration::from_micros(microseconds)));
    }
    assert_eq!(marker.checks(), 1);
    assert!(marker.exited(start + INTERVAL));
    std::fs::remove_file(path).expect("external cleanup after exit");
    assert!(marker.exited(start + Duration::from_secs(1)));
    assert_eq!(
        marker.checks(),
        2,
        "draining must not touch the filesystem again"
    );
}

#[test]
fn a_late_iteration_does_not_catch_up_with_a_burst_of_shared_filesystem_calls() {
    let directory = tempfile::tempdir().expect("private marker directory");
    let start = Instant::now();
    let mut marker = ExitMarker::new(directory.path().join("absent"), start);
    assert!(!marker.exited(start));
    let after_stall = start + Duration::from_secs(1);
    assert!(!marker.exited(after_stall));
    for _ in 0..1000 {
        assert!(!marker.exited(after_stall));
    }
    assert_eq!(marker.checks(), 2);
    assert!(!marker.exited(after_stall + INTERVAL));
    assert_eq!(marker.checks(), 3);
}
