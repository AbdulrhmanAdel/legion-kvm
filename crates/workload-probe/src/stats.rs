//! CPU time and scheduler evidence; no host setting is changed.
use std::{fs, time::Duration};

use rustix::time::{ClockId, clock_gettime};
use serde::Serialize;

pub fn cpu_time(thread: bool) -> Duration {
    let t = clock_gettime(if thread {
        ClockId::ThreadCPUTime
    } else {
        ClockId::ProcessCPUTime
    });
    Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
}

#[derive(Default, Clone, Debug, Serialize)]
pub struct Scheduler {
    pub voluntary_switches: u64,
    pub involuntary_switches: u64,
    pub runtime_ns: u64,
    pub runqueue_ns: Option<u64>,
    pub timeslices: u64,
}

impl Scheduler {
    pub fn current() -> Self {
        let status = fs::read_to_string("/proc/thread-self/status").unwrap_or_default();
        let stat = fs::read_to_string("/proc/thread-self/schedstat").unwrap_or_default();
        let enabled =
            fs::read_to_string("/proc/sys/kernel/sched_schedstats").is_ok_and(|s| s.trim() == "1");
        Self::parse(&status, &stat, enabled)
    }

    fn parse(status: &str, stat: &str, enabled: bool) -> Self {
        let field = |name| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0)
        };
        let times: Vec<u64> = stat
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        Self {
            voluntary_switches: field("voluntary_ctxt_switches:"),
            involuntary_switches: field("nonvoluntary_ctxt_switches:"),
            runtime_ns: times.first().copied().unwrap_or(0),
            runqueue_ns: enabled.then(|| times.get(1).copied().unwrap_or(0)),
            timeslices: times.get(2).copied().unwrap_or(0),
        }
    }

    pub fn since(&self, before: &Self) -> Self {
        Self {
            voluntary_switches: self
                .voluntary_switches
                .saturating_sub(before.voluntary_switches),
            involuntary_switches: self
                .involuntary_switches
                .saturating_sub(before.involuntary_switches),
            runtime_ns: self.runtime_ns.saturating_sub(before.runtime_ns),
            runqueue_ns: self
                .runqueue_ns
                .zip(before.runqueue_ns)
                .map(|(a, b)| a.saturating_sub(b)),
            timeslices: self.timeslices.saturating_sub(before.timeslices),
        }
    }
}

pub fn distribution(values: &mut [u64]) -> serde_json::Value {
    values.sort_unstable();
    if values.is_empty() {
        return serde_json::json!({"samples":0});
    }
    let at = |q: f64| values[((values.len() - 1) as f64 * q) as usize] as f64 / 1e6;
    serde_json::json!({"samples":values.len(), "mean_ms":values.iter().map(|&n| n as u128).sum::<u128>() as f64/values.len() as f64/1e6, "p50_ms":at(0.5), "p95_ms":at(0.95), "p99_ms":at(0.99), "max_ms":at(1.0)})
}

pub fn memory_pages() -> serde_json::Value {
    let fields = [
        "Rss",
        "AnonHugePages",
        "ShmemPmdMapped",
        "FilePmdMapped",
        "Shared_Hugetlb",
        "Private_Hugetlb",
    ];
    let mut values = serde_json::Map::new();
    if let Ok(text) = fs::read_to_string("/proc/self/smaps_rollup") {
        for line in text.lines() {
            if let Some((name, value)) = line.split_once(':')
                && fields.contains(&name)
            {
                values.insert(
                    format!("{name}_kib"),
                    value
                        .split_whitespace()
                        .next()
                        .and_then(|v| v.parse::<u64>().ok())
                        .map_or(serde_json::Value::Null, Into::into),
                );
            }
        }
    }
    values.insert(
        "thp_policy".into(),
        fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
            .ok()
            .map_or(serde_json::Value::Null, |s| s.trim().into()),
    );
    values.insert(
        "shmem_thp_policy".into(),
        fs::read_to_string("/sys/kernel/mm/transparent_hugepage/shmem_enabled")
            .ok()
            .map_or(serde_json::Value::Null, |s| s.trim().into()),
    );
    serde_json::Value::Object(values)
}

#[cfg(test)]
#[path = "../tests/unit/stats.rs"]
mod tests;
