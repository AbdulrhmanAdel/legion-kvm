//! Submission/completion exchange, with a separate-RPC comparison control.
use std::fs::File;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use legion_display_channel::rpc::{EXCHANGE_BYTES, FRAME_BYTES, Frame, RELEASE_BYTES, Releases};
use rustix::event::{PollFd, PollFlags, poll};
use serde_json::{Value, json};
use smithay::reexports::wayland_server::{Resource, protocol::wl_buffer::WlBuffer};
use std::os::fd::AsFd;

pub(crate) struct Cached {
    pub source: WlBuffer,
    pub id: u64,
    pub frame: Option<u64>,
    pub acquire_started: Option<Instant>,
    pub submitted: Option<Instant>,
    pub retired: bool,
    pub release_point: Option<crate::syncobj::Release>,
}

pub(crate) fn ioctl(fd: &File, cmd: u32, data: &mut [u8]) -> Result<()> {
    ensure!(
        ((cmd >> 16) & 0x3fff) as usize == data.len(),
        "ioctl buffer size mismatch"
    );
    // SAFETY: these pointer-free display/DRM core ioctls copy exactly _IOC_SIZE bytes into/from this live writable slice and retain no pointer.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), cmd as libc::Ioctl, data.as_mut_ptr()) };
    ensure!(
        result >= 0,
        "display ioctl 0x{cmd:x}: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

pub(crate) fn call(fd: &File, nr: u32, data: &mut [u8]) -> Result<()> {
    ioctl(
        fd,
        0xc000_4c00 | (u32::try_from(data.len())? << 16) | nr,
        data,
    )
}

fn wait_event_accounting(
    frames: u64,
    timeout: Duration,
    mut read: impl FnMut(&mut [u8; 24]) -> Result<()>,
) -> Result<[u8; 24]> {
    let started = Instant::now();
    loop {
        let mut events = [0; 24];
        read(&mut events)?;
        let acknowledged = u64::from_le_bytes(events[8..16].try_into()?);
        ensure!(
            acknowledged <= frames,
            "host acknowledged {acknowledged} events for {frames} frames"
        );
        if acknowledged == frames {
            return Ok(events);
        }
        ensure!(
            started.elapsed() < timeout,
            "host event accounting timed out: acknowledged={acknowledged}, frames={frames}"
        );
        std::thread::sleep(Duration::from_micros(100));
    }
}

pub(crate) fn final_event_accounting(fd: &File, frames: u64) -> Result<[u8; 24]> {
    // The guest can consume a release before the host event thread updates its counters.
    wait_event_accounting(frames, Duration::from_secs(3), |events| call(fd, 9, events))
}

#[derive(Default)]
struct Timing {
    calls: u64,
    ns: u128,
    max_ns: u128,
}
impl Timing {
    fn report(&self) -> Value {
        json!({"calls": self.calls, "mean_ms": if self.calls == 0 { 0.0 } else { self.ns as f64 / self.calls as f64 / 1e6 }, "max_ms": self.max_ns as f64 / 1e6})
    }
}
fn measured(fd: &File, nr: u32, data: &mut [u8], timing: &mut Timing) -> Result<()> {
    let begin = Instant::now();
    let result = call(fd, nr, data);
    let ns = begin.elapsed().as_nanos();
    timing.calls += 1;
    timing.ns += ns;
    timing.max_ns = timing.max_ns.max(ns);
    result
}

fn apply(
    batch: &Releases,
    images: &mut [Cached],
    host_lease: &mut Timing,
    lifetime: &mut Timing,
) -> Result<u64> {
    // Validate the entire reply before releasing even its first source buffer.
    batch.validate(|id| {
        images
            .iter()
            .find(|image| image.id == id)
            .filter(|image| image.submitted.is_some() && image.acquire_started.is_some())
            .and_then(|image| image.frame)
    })?;
    let now = Instant::now();
    for release in batch.as_slice() {
        let image = images
            .iter_mut()
            .find(|image| image.id == release.resource)
            .expect("validated resource still exists");
        image.frame = None;
        for (start, timing) in [
            (image.submitted.take(), &mut *host_lease),
            (image.acquire_started.take(), &mut *lifetime),
        ] {
            let ns = now
                .duration_since(start.expect("validated lease timestamp"))
                .as_nanos();
            timing.calls += 1;
            timing.ns += ns;
            timing.max_ns = timing.max_ns.max(ns);
        }
        if let Some(point) = image.release_point.take() {
            point.signal().context("signal forwarded buffer release")?;
        }
        if image.source.is_alive() {
            image.source.release();
        }
    }
    Ok(u64::try_from(batch.as_slice().len())?)
}

fn poll_due(
    combined: bool,
    elapsed: Duration,
    pending_source: bool,
    leased: bool,
    fresh_release: bool,
) -> bool {
    if combined {
        leased && (elapsed >= Duration::from_micros(100) || (pending_source && !fresh_release))
    } else {
        pending_source || elapsed >= Duration::from_micros(100)
    }
}

fn notification_ready(fd: &impl AsFd) -> Result<bool> {
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    poll(
        &mut fds,
        Some(&rustix::time::Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        }),
    )?;
    let events = fds[0].revents();
    ensure!(
        !events.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL),
        "display notification descriptor failed"
    );
    Ok(events.contains(PollFlags::IN))
}

pub(crate) struct Transport {
    combined: bool,
    notified: bool,
    polled: bool,
    last_reply: Instant,
    present: Timing,
    exchange: Timing,
    release: Timing,
    empty_polls: u64,
    piggybacked: u64,
    standalone: u64,
    fresh_release: bool,
    force_pull: bool,
    readiness: Timing,
    notifications: u64,
    host_lease: Timing,
    lifetime: Timing,
}
impl Transport {
    pub fn new(mode: &str) -> Result<Self> {
        ensure!(
            matches!(mode, "polled" | "notified" | "combined" | "separate"),
            "unknown frame transport"
        );
        Ok(Self {
            combined: mode != "separate",
            notified: matches!(mode, "polled" | "notified"),
            polled: mode == "polled",
            last_reply: Instant::now(),
            present: Timing::default(),
            exchange: Timing::default(),
            release: Timing::default(),
            empty_polls: 0,
            piggybacked: 0,
            standalone: 0,
            fresh_release: false,
            force_pull: false,
            readiness: Timing::default(),
            notifications: 0,
            host_lease: Timing::default(),
            lifetime: Timing::default(),
        })
    }
    pub fn is_combined(&self) -> bool {
        self.combined
    }
    pub fn is_polled(&self) -> bool {
        self.polled
    }
    pub fn is_notified(&self) -> bool {
        self.notified
    }
    pub fn mode(&self) -> &'static str {
        if self.polled {
            "polled"
        } else if self.notified {
            "notified"
        } else if self.combined {
            "combined"
        } else {
            "separate"
        }
    }

    pub fn submit(
        &mut self,
        fd: &File,
        images: &mut [Cached],
        resource: u64,
        frame: u64,
    ) -> Result<u64> {
        let index = images
            .iter()
            .position(|image| image.id == resource)
            .context("unknown submission resource")?;
        ensure!(
            images[index].frame.is_none() && !images[index].retired,
            "submission resource is still leased or retired"
        );
        let identity = Frame { resource, frame };
        let encoded = identity.encode()?;
        ensure!(
            images[index].acquire_started.is_some(),
            "missing acquire timestamp"
        );
        images[index].submitted = Some(Instant::now());
        if self.combined && !self.notified {
            let mut packet = [0; EXCHANGE_BYTES];
            packet[..FRAME_BYTES].copy_from_slice(&encoded);
            measured(fd, 7, &mut packet, &mut self.exchange)?;
            ensure!(
                Frame::decode(&packet[..FRAME_BYTES])? == identity,
                "submission echo mismatch"
            );
            // The host may finish this very submission before replying.
            images[index].frame = Some(frame);
            let returned = apply(
                &Releases::decode(&packet[FRAME_BYTES..])?,
                images,
                &mut self.host_lease,
                &mut self.lifetime,
            )?;
            self.force_pull = returned == 16;
            self.piggybacked += returned;
            if returned > 0 {
                self.last_reply = Instant::now();
                self.fresh_release = true;
            }
            Ok(returned)
        } else {
            let mut packet = encoded;
            measured(
                fd,
                if self.polled { 10 } else { 3 },
                &mut packet,
                &mut self.present,
            )?;
            images[index].frame = Some(frame);
            Ok(0)
        }
    }

    pub fn pull(&mut self, fd: &File, images: &mut [Cached]) -> Result<u64> {
        let mut packet = [0; RELEASE_BYTES];
        measured(
            fd,
            if self.notified { 8 } else { 4 },
            &mut packet,
            &mut self.release,
        )?;
        let returned = apply(
            &Releases::decode(&packet)?,
            images,
            &mut self.host_lease,
            &mut self.lifetime,
        )?;
        self.force_pull = returned == 16;
        self.empty_polls += u64::from(returned == 0);
        self.standalone += returned;
        self.fresh_release = returned > 0;
        self.last_reply = Instant::now();
        Ok(returned)
    }

    pub fn poll_if_due(
        &mut self,
        fd: &File,
        images: &mut [Cached],
        pending_source: bool,
    ) -> Result<u64> {
        if self.notified {
            if !images.iter().any(|image| image.frame.is_some()) {
                return Ok(0);
            }
            let begin = Instant::now();
            let ready = notification_ready(fd)?;
            let ns = begin.elapsed().as_nanos();
            self.readiness.calls += 1;
            self.readiness.ns += ns;
            self.readiness.max_ns = self.readiness.max_ns.max(ns);
            self.notifications += u64::from(ready);
            return if ready
                || self.force_pull
                || self.last_reply.elapsed() >= Duration::from_millis(1)
            {
                self.pull(fd, images)
            } else {
                Ok(0)
            };
        }
        let due = poll_due(
            self.combined,
            self.last_reply.elapsed(),
            pending_source,
            images.iter().any(|image| image.frame.is_some()),
            self.fresh_release,
        );
        if self.combined && pending_source {
            self.fresh_release = false;
        }
        if due { self.pull(fd, images) } else { Ok(0) }
    }

    pub fn report(&self) -> Value {
        json!({"mode":self.mode(), "present":self.present.report(), "exchange":self.exchange.report(), "release":self.release.report(), "total_calls":self.present.calls+self.exchange.calls+if self.notified { 0 } else { self.release.calls }, "submission_scope":if self.polled { "queued" } else { "host_roundtrip" }, "release_scope":if self.notified { "guest_local" } else { "host_rpc" }, "local_release_reads":if self.notified { self.release.calls } else { 0 }, "empty_release_calls":self.empty_polls, "piggybacked_acks":self.piggybacked, "standalone_acks":self.standalone, "release_poll_interval_us":if self.notified { 1000 } else { 100 }, "readiness_checks":self.readiness.report(), "notifications_received":self.notifications, "host_lease":self.host_lease.report(), "acquire_to_output_release":self.lifetime.report()})
    }
}

#[cfg(test)]
#[path = "../tests/unit/frame_transport.rs"]
mod tests;
