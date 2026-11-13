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
#[path = "../tests/unit/exit_marker.rs"]
mod tests;
