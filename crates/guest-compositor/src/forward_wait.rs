//! Opt-in event waits for the forwarding loops; GPU fences remain polled.
use std::{os::fd::BorrowedFd, time::Instant};

use anyhow::{Result, ensure};
use rustix::event::{PollFd, PollFlags, poll};
use serde_json::{Value, json};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Spin,
    Events,
    Hybrid,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Spin => "spin",
            Self::Events => "events",
            Self::Hybrid => "hybrid",
        }
    }
}

pub(crate) struct ForwardWait {
    mode: Mode,
    yield_calls: u64,
    calls: u64,
    timeouts: u64,
    elapsed_ns: u128,
    cpu_start_ns: u64,
    idle_retry_us: u64,
}

fn process_cpu_ns() -> Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes one initialized, live timespec and retains no pointer.
    let status = unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) };
    ensure!(
        status == 0,
        "read compositor CPU clock: {}",
        std::io::Error::last_os_error()
    );
    Ok(u64::try_from(time.tv_sec)? * 1_000_000_000 + u64::try_from(time.tv_nsec)?)
}

impl ForwardWait {
    pub fn new(config: &Value) -> Result<Self> {
        let mode = config
            .get("forward_wait")
            .map_or(Some("spin"), Value::as_str);
        let mode = match mode {
            Some("spin") => Mode::Spin,
            Some("events") => Mode::Events,
            Some("hybrid") => Mode::Hybrid,
            _ => anyhow::bail!("unknown forwarding wait mode"),
        };
        let idle_retry_us = config
            .get("forward_idle_retry_us")
            .map_or(Some(1000), Value::as_u64)
            .filter(|value| (100..=1000).contains(value))
            .ok_or_else(|| anyhow::anyhow!("idle retry must be 100..=1000 microseconds"))?;
        Ok(Self {
            mode,
            yield_calls: 0,
            calls: 0,
            timeouts: 0,
            elapsed_ns: 0,
            cpu_start_ns: process_cpu_ns()?,
            idle_retry_us,
        })
    }

    pub fn wait<const N: usize>(
        &mut self,
        descriptors: [BorrowedFd<'_>; N],
        gpu_pending: bool,
    ) -> Result<()> {
        if self.mode == Mode::Spin || (self.mode == Mode::Hybrid && gpu_pending) {
            // Pending acquisition already has real GPU work in flight.
            self.yield_calls += 1;
            std::thread::yield_now();
            return Ok(());
        }
        ensure!(
            !descriptors.is_empty(),
            "forwarding wait has no descriptors"
        );
        let mut fds = descriptors.map(|fd| PollFd::from_borrowed_fd(fd, PollFlags::IN));
        // GL acquisition fences have no pollable FD in the current path.
        let timeout = rustix::time::Timespec {
            tv_sec: 0,
            tv_nsec: if gpu_pending {
                100_000
            } else {
                self.idle_retry_us as i64 * 1000
            },
        };
        let started = Instant::now();
        match poll(&mut fds, Some(&timeout)) {
            Ok(ready) => {
                self.timeouts += u64::from(ready == 0);
                ensure!(
                    fds.iter().all(|fd| !fd
                        .revents()
                        .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL)),
                    "forwarding wait descriptor disconnected"
                );
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
        self.calls += 1;
        self.elapsed_ns += started.elapsed().as_nanos();
        Ok(())
    }

    pub fn report(&self) -> Result<Value> {
        Ok(
            json!({"mode":self.mode.name(),"yield_calls":self.yield_calls,
            "wait_calls":self.calls,"timeouts":self.timeouts,
            "wait_seconds":self.elapsed_ns as f64 / 1e9,
            "process_cpu_seconds":process_cpu_ns()?.saturating_sub(self.cpu_start_ns) as f64 / 1e9,
            "gpu_retry_us":if self.mode == Mode::Events {100} else {0},"idle_retry_us":self.idle_retry_us}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        os::{fd::AsFd, unix::net::UnixStream},
    };

    #[test]
    fn unreadable_socket_times_out_but_readable_socket_wakes_the_wait() {
        let (read, mut write) = UnixStream::pair().expect("owned sockets");
        let mut waiter = ForwardWait::new(&json!({"forward_wait":"events"})).expect("event mode");
        waiter
            .wait([read.as_fd()], true)
            .expect("bounded fence retry");
        write.write_all(b"ready").expect("publish event");
        waiter
            .wait([read.as_fd()], false)
            .expect("immediate socket readiness");
        let report = waiter.report().expect("CPU clock");
        assert_eq!(report["wait_calls"], 2);
        assert_eq!(report["timeouts"], 1);
    }

    #[test]
    fn mode_defaults_to_spin_and_rejects_invalid_settings() {
        let mut waiter = ForwardWait::new(&json!({})).expect("original mode");
        waiter.wait([], false).expect("original yield");
        assert_eq!(waiter.report().expect("CPU clock")["wait_calls"], 0);
        for value in [json!(null), json!(true), json!("unknown")] {
            assert!(ForwardWait::new(&json!({"forward_wait":value})).is_err());
        }
        for value in [
            json!(null),
            json!(true),
            json!(99),
            json!(1001),
            json!("100"),
        ] {
            assert!(ForwardWait::new(&json!({"forward_idle_retry_us":value})).is_err());
        }
        for retry in [100, 250, 1000] {
            let waiter =
                ForwardWait::new(&json!({"forward_wait":"events", "forward_idle_retry_us":retry}))
                    .expect("bounded retry");
            assert_eq!(waiter.report().expect("report")["idle_retry_us"], retry);
        }
    }

    #[test]
    fn hybrid_yields_for_pending_gpu_work_but_waits_for_idle_socket_events() {
        let mut waiter = ForwardWait::new(&json!({"forward_wait":"hybrid"})).expect("hybrid mode");
        waiter
            .wait([], true)
            .expect("GPU completion stays immediately queryable");
        assert!(
            waiter.wait([], false).is_err(),
            "idle waits need actual event descriptors"
        );
        let (read, mut write) = UnixStream::pair().expect("owned sockets");
        waiter
            .wait([read.as_fd()], false)
            .expect("bounded idle retry");
        write.write_all(b"ready").expect("publish socket event");
        waiter
            .wait([read.as_fd()], false)
            .expect("socket interrupts idle wait");
        let report = waiter.report().expect("CPU clock");
        assert_eq!(report["mode"], "hybrid");
        assert_eq!(report["yield_calls"], 1);
        assert_eq!(report["wait_calls"], 2);
        assert_eq!(report["timeouts"], 1);
        assert_eq!(report["gpu_retry_us"], 0);
    }
}
