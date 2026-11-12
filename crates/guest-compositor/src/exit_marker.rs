//! Bound shared-filesystem exit checks independently of GPU-fence polling.
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

pub(crate) const INTERVAL: Duration = Duration::from_millis(10);

pub(crate) struct ExitMarker {
    path: PathBuf,
    next_check: Instant,
    observed: bool,
    checks: u64,
    timing: crate::frame_timing::Distribution,
}

impl ExitMarker {
    pub(crate) fn new(path: impl Into<PathBuf>, now: Instant) -> Self {
        Self {
            path: path.into(),
            next_check: now,
            observed: false,
            checks: 0,
            timing: crate::frame_timing::Distribution::default(),
        }
    }

    pub(crate) fn exited(&mut self, now: Instant) -> bool {
        if !self.observed && now >= self.next_check {
            let started = Instant::now();
            self.observed = self.path.is_file();
            self.timing.record(started.elapsed());
            self.checks += 1;
            // Schedule from this observation, rather than catching up missed deadlines in a burst after a scheduler stall.
            self.next_check = now + INTERVAL;
        }
        self.observed
    }

    pub(crate) fn checks(&self) -> u64 {
        self.checks
    }

    pub(crate) fn take_timing(&mut self) -> serde_json::Value {
        self.timing.take()
    }
}

#[cfg(test)]
mod tests {
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
}
