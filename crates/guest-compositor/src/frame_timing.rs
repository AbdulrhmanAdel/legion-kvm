//! Bounded one-second timing windows.
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const CAPACITY: usize = 4096;

pub(crate) struct Distribution {
    samples: Vec<u64>,
    count: u64,
    total_ns: u128,
    max_ns: u64,
    over_16ms: u64,
    over_33ms: u64,
    over_50ms: u64,
}

impl Default for Distribution {
    fn default() -> Self {
        Self {
            samples: Vec::with_capacity(CAPACITY),
            count: 0,
            total_ns: 0,
            max_ns: 0,
            over_16ms: 0,
            over_33ms: 0,
            over_50ms: 0,
        }
    }
}

impl Distribution {
    pub(crate) fn record(&mut self, elapsed: Duration) {
        let ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        self.count += 1;
        self.total_ns += u128::from(ns);
        self.max_ns = self.max_ns.max(ns);
        self.over_16ms += u64::from(ns > 16_666_667);
        self.over_33ms += u64::from(ns > 33_333_333);
        self.over_50ms += u64::from(ns > 50_000_000);
        if self.samples.len() < CAPACITY {
            self.samples.push(ns);
        }
    }

    pub(crate) fn take(&mut self) -> Value {
        self.samples.sort_unstable();
        // Omitted percentiles explicitly mark an empty or truncated window.
        let complete = self.count != 0 && self.count == self.samples.len() as u64;
        let quantile = |percent: usize| {
            complete.then(|| {
                let index = (self.samples.len() * percent).div_ceil(100) - 1;
                self.samples[index] as f64 / 1e6
            })
        };
        let report = json!({
            "count":self.count,"dropped_samples":self.count-self.samples.len() as u64,
            "p50_ms":quantile(50),"p95_ms":quantile(95),"p99_ms":quantile(99),
            "mean_ms":(self.count!=0).then(||self.total_ns as f64/self.count as f64/1e6),
            "max_ms":(self.count!=0).then(||self.max_ns as f64/1e6),
            "over_16_67_ms":self.over_16ms,"over_33_33_ms":self.over_33ms,"over_50_ms":self.over_50ms,
        });
        self.samples.clear();
        self.count = 0;
        self.total_ns = 0;
        self.max_ns = 0;
        self.over_16ms = 0;
        self.over_33ms = 0;
        self.over_50ms = 0;
        report
    }
}

#[derive(Default)]
pub(crate) struct Cadence {
    previous: Option<Instant>,
    intervals: Distribution,
}

impl Cadence {
    pub(crate) fn record(&mut self, now: Instant) {
        if let Some(previous) = self.previous.replace(now) {
            self.intervals
                .record(now.saturating_duration_since(previous));
        }
    }
    pub(crate) fn take(&mut self) -> Value {
        self.intervals.take()
    }
    pub(crate) fn reset_clock(&mut self) {
        self.previous = None;
    }
}

#[cfg(test)]
#[path = "../tests/unit/frame_timing.rs"]
mod tests;
