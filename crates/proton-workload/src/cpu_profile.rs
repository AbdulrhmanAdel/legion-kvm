//! Opt-in CPU-clock sampling of the owned PE's main thread, inside its namespace.
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::io::Read;
use std::{
    fs, io, mem,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    ptr::NonNull,
    sync::{
        atomic::{Ordering, fence},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_SAMPLES: usize = 100_000;
// Linux x86 perf register indices: AX..IP/FLAGS and R8..R15; omit segment registers.
const REG_MASK: u64 = 0x00ff_03ff;
const STACK_BYTES: u32 = 128;

#[cfg(feature = "driver-context-probe")]
#[path = "driver_context.rs"]
mod driver_context_probe;
#[cfg(feature = "syscall-count-probe")]
#[path = "syscall_counts.rs"]
mod syscall_probe;

#[repr(C)]
#[derive(Default)]
struct Attr {
    kind: u32,
    size: u32,
    config: u64,
    period: u64,
    sample: u64,
    read_format: u64,
    flags: u64,
    wakeup: u32,
    bp_type: u32,
    config1: u64,
    config2: u64,
    branch_sample: u64,
    regs: u64,
    stack: u32,
    clock: i32,
}
const _: () = assert!(mem::size_of::<Attr>() == 96);

fn sampling_attribute() -> Attr {
    Attr {
        kind: 1,
        size: mem::size_of::<Attr>() as u32,
        period: 500_000,
        sample: 1 | 2 | 4 | (1 << 12),
        flags: (1 << 5) | (1 << 6) | (1 << 25),
        clock: libc::CLOCK_MONOTONIC,
        wakeup: 1,
        regs: REG_MASK,
        ..Attr::default()
    }
}

struct Counters(Vec<(&'static str, Result<fs::File, String>)>);
impl Counters {
    fn open(pid: i32) -> Self {
        Self(
            [
                ("cycles", 0),
                ("instructions", 1),
                ("cache_misses", 3),
                ("branch_misses", 5),
            ]
            .into_iter()
            .map(|(name, config)| {
                let attr = Attr {
                    kind: 0,
                    size: mem::size_of::<Attr>() as u32,
                    config,
                    read_format: 3,
                    flags: (1 << 5) | (1 << 6),
                    ..Attr::default()
                };
                // SAFETY: counting only the same verified owned main thread, userspace only, no inheritance, sampling or system-wide scope.
                let fd =
                    unsafe { libc::syscall(libc::SYS_perf_event_open, &attr, pid, -1, -1, 8_u64) };
                let result = if fd < 0 {
                    Err(io::Error::last_os_error().to_string())
                } else {
                    // SAFETY: fresh descriptor returned by the successful syscall.
                    Ok(unsafe { fs::File::from_raw_fd(fd as i32) })
                };
                (name, result)
            })
            .collect(),
        )
    }
    fn read(&self) -> serde_json::Value {
        let mut values = serde_json::Map::new();
        for (name, file) in &self.0 {
            let value = match file {
                Ok(file) => {
                    let mut buffer = [0; 24];
                    match (&*file).read_exact(&mut buffer) {
                        Ok(()) => {
                            serde_json::json!({"value":u64::from_ne_bytes(buffer[..8].try_into().expect("fixed buffer")),
                            "enabled_ns":u64::from_ne_bytes(buffer[8..16].try_into().expect("fixed buffer")),
                            "running_ns":u64::from_ne_bytes(buffer[16..24].try_into().expect("fixed buffer"))})
                        }
                        Err(e) => serde_json::json!({"unavailable":e.to_string()}),
                    }
                }
                Err(e) => serde_json::json!({"unavailable":e}),
            };
            values.insert((*name).into(), value);
        }
        serde_json::Value::Object(values)
    }
}

#[derive(Serialize)]
struct Sample {
    ip: u64,
    pid: u32,
    tid: u32,
    time_ns: u64,
    abi: u64,
    regs: Vec<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stack_words: Vec<u64>,
}

fn decode_sample(bytes: &[u8]) -> Result<Sample> {
    decode_sample_with_stack(bytes, false)
}

fn decode_sample_with_stack(bytes: &[u8], with_stack: bool) -> Result<Sample> {
    ensure!(bytes.len() >= 40, "short CPU sample");
    let u64_at = |i| u64::from_ne_bytes(bytes[i..i + 8].try_into().expect("checked fixed field"));
    let abi = u64_at(32);
    ensure!(abi <= 2, "unexpected sampled register ABI {abi}");
    let count = if abi != 0 {
        REG_MASK.count_ones() as usize
    } else {
        0
    };
    let end = 40 + count * 8;
    ensure!(bytes.len() >= end, "short CPU registers");
    let mut stack_words = Vec::new();
    if with_stack {
        ensure!(bytes.len() >= end + 8, "missing CPU stack extent");
        let size = usize::try_from(u64_at(end))?;
        ensure!(
            size <= STACK_BYTES as usize,
            "CPU stack exceeds capture bound"
        );
        let trailer = usize::from(size > 0) * 8;
        ensure!(
            bytes.len() == end + 8 + size + trailer,
            "CPU stack length mismatch"
        );
        let valid = if size > 0 {
            usize::try_from(u64_at(end + 8 + size))?
        } else {
            0
        };
        ensure!(valid <= size, "invalid CPU stack dynamic extent");
        stack_words = bytes[end + 8..end + 8 + valid]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| u64::from_ne_bytes(*word))
            .collect();
    } else {
        ensure!(bytes.len() == end, "CPU register sample length mismatch");
    }
    Ok(Sample {
        ip: u64_at(8),
        pid: u32::from_ne_bytes(bytes[16..20].try_into()?),
        tid: u32::from_ne_bytes(bytes[20..24].try_into()?),
        time_ns: u64_at(24),
        abi,
        regs: (0..count).map(|i| u64_at(40 + i * 8)).collect(),
        stack_words,
    })
}

fn ring_segments(size: usize, tail: u64, length: usize) -> Result<(usize, usize)> {
    ensure!(
        size.is_power_of_two() && length <= size,
        "invalid perf ring record size"
    );
    let offset = tail as usize & (size - 1);
    Ok((offset, length.min(size - offset)))
}

struct Ring {
    base: NonNull<u8>,
    length: usize,
    offset: usize,
    size: usize,
    tail: u64,
    _fd: OwnedFd,
    with_stack: bool,
}
impl Ring {
    fn open(pid: i32, with_stack: bool) -> Result<Self> {
        let mut attr = sampling_attribute();
        if with_stack {
            attr.sample |= 1 << 13;
            attr.stack = STACK_BYTES;
        }
        Self::from_attribute(pid, attr, with_stack)
    }
    fn from_attribute(pid: i32, attr: Attr, with_stack: bool) -> Result<Self> {
        // SAFETY: initialized perf UAPI v3 descriptor, positive verified owned thread ID, no CPU/system-wide scope or inheritance, CLOEXEC descriptor.
        let raw = unsafe { libc::syscall(libc::SYS_perf_event_open, &attr, pid, -1, -1, 8_u64) };
        ensure!(
            raw >= 0,
            "owned-thread perf_event_open: {}",
            io::Error::last_os_error()
        );
        // SAFETY: the syscall returned a fresh owned descriptor, checked above.
        let fd = unsafe { OwnedFd::from_raw_fd(i32::try_from(raw)?) };
        // SAFETY: sysconf has no pointer arguments and returns the page size.
        let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })?;
        ensure!(
            page >= 4096 && page.is_power_of_two(),
            "unsupported perf page size"
        );
        let length = page * 9;
        // SAFETY: live perf fd, documented metadata page plus eight data pages.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        ensure!(
            pointer != libc::MAP_FAILED,
            "perf mmap: {}",
            io::Error::last_os_error()
        );
        let base = NonNull::new(pointer.cast()).ok_or_else(|| anyhow::anyhow!("null perf ring"))?;
        let mut ring = Self {
            base,
            length,
            offset: page,
            size: page * 8,
            tail: 0,
            _fd: fd,
            with_stack,
        };
        // SAFETY: the mapped metadata page covers these aligned UAPI u64 fields.
        let (offset, size) = unsafe {
            (
                ring.word(1040).read_volatile(),
                ring.word(1048).read_volatile(),
            )
        };
        ring.offset = usize::try_from(offset)?;
        ring.size = usize::try_from(size)?;
        ensure!(
            ring.offset >= page
                && ring.size.is_power_of_two()
                && ring
                    .offset
                    .checked_add(ring.size)
                    .is_some_and(|end| end <= length),
            "invalid perf mapping metadata"
        );
        Ok(ring)
    }
    /// # Safety `offset` must name an aligned u64 fully inside the live mapped metadata page.
    unsafe fn word(&self, offset: usize) -> *mut u64 {
        // SAFETY: caller upholds metadata offset/alignment and mapping lifetime.
        unsafe { self.base.as_ptr().add(offset).cast() }
    }
    fn copy_published(&self, tail: u64, length: usize) -> Result<Vec<u8>> {
        let (offset, first) = ring_segments(self.size, tail, length)?;
        let mut copy = vec![0; length];
        // SAFETY: caller drains only bytes within acquired head, retaining tail until the copy completes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.base.as_ptr().add(self.offset + offset),
                copy.as_mut_ptr(),
                first,
            );
            std::ptr::copy_nonoverlapping(
                self.base.as_ptr().add(self.offset),
                copy.as_mut_ptr().add(first),
                length - first,
            );
        }
        Ok(copy)
    }
    fn drain(&mut self, samples: &mut Vec<Sample>, lost: &mut u64) -> Result<()> {
        let with_stack = self.with_stack;
        self.drain_records(
            |record| {
                ensure!(samples.len() < MAX_SAMPLES, "CPU sample cap reached");
                samples.push(if with_stack {
                    decode_sample_with_stack(record, true)?
                } else {
                    decode_sample(record)?
                });
                Ok(())
            },
            lost,
        )
    }
    fn drain_records(
        &mut self,
        mut sample: impl FnMut(&[u8]) -> Result<()>,
        lost: &mut u64,
    ) -> Result<()> {
        // SAFETY: documented aligned data_head field in the live metadata page.
        let head = unsafe { self.word(1024).read_volatile() };
        fence(Ordering::Acquire);
        ensure!(
            head >= self.tail && head - self.tail <= self.size as u64,
            "perf ring overwritten"
        );
        while self.tail < head {
            ensure!(head - self.tail >= 8, "partial perf record header");
            let header = self.copy_published(self.tail, 8)?;
            let kind = u32::from_ne_bytes(header[..4].try_into()?);
            let size = usize::from(u16::from_ne_bytes(header[6..8].try_into()?));
            ensure!(
                size >= 8 && size as u64 <= head - self.tail,
                "invalid published perf record"
            );
            let record = self.copy_published(self.tail, size)?;
            if kind == 9 {
                sample(&record)?;
            } else if kind == 2 {
                ensure!(record.len() >= 24, "short lost-sample record");
                *lost += u64::from_ne_bytes(record[16..24].try_into()?);
            }
            self.tail += size as u64;
        }
        // Full barrier: data reads must complete before kernel may reuse the ring.
        fence(Ordering::SeqCst);
        // SAFETY: unique consumer writes the documented aligned data_tail field.
        unsafe {
            self.word(1032).write_volatile(self.tail);
        }
        Ok(())
    }
}
impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: exactly this base/length came from the successful mmap owned here.
        unsafe {
            libc::munmap(self.base.as_ptr().cast(), self.length);
        }
    }
}

fn now_ns() -> Result<u64> {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: writable timespec outlives this synchronous clock query.
    let status = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    ensure!(status == 0, "CPU profile clock failed");
    Ok(u64::try_from(t.tv_sec)? * 1_000_000_000 + u64::try_from(t.tv_nsec)?)
}

fn has_fixture_image(maps: &str) -> bool {
    maps.lines()
        .any(|line| line.split_whitespace().nth(5) == Some("/usr/share/legion/workload.exe"))
}

fn marker_ids(bytes: &[u8]) -> Option<(i32, i32)> {
    let marker: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if marker["schema"] != 1 {
        return None;
    }
    let pid = i32::try_from(marker["pid"].as_i64()?).ok()?;
    let tid = i32::try_from(marker["tid"].as_i64()?).ok()?;
    (pid > 0 && tid > 0).then_some((pid, tid))
}

fn image_pid() -> Result<Option<i32>> {
    let mut found = None;
    for process in fs::read_dir("/proc")? {
        let process = process?;
        let Ok(pid) = process.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(maps) = fs::read_to_string(process.path().join("maps")) else {
            continue;
        };
        if has_fixture_image(&maps) {
            ensure!(
                found.is_none(),
                "multiple owned workload images; refusing ambiguous CPU profile"
            );
            found = Some(pid);
        }
    }
    Ok(found)
}

#[derive(Clone, Copy)]
enum Target {
    Main,
    Present,
    Syscalls,
    QueuePlacement { colocate: bool, server: bool },
}

fn collect_profile(receive: mpsc::Receiver<()>, target: Target) -> Result<()> {
    #[cfg(not(feature = "syscall-count-probe"))]
    if let Target::QueuePlacement { colocate, server } = target {
        anyhow::bail!(
            "queue placement ({colocate}, server={server}) requires the development Cargo feature"
        );
    }
    let present_worker = matches!(target, Target::Present);
    let syscall_counts = matches!(target, Target::Syscalls | Target::QueuePlacement { .. });
    let deadline = Instant::now() + Duration::from_secs(20);
    let (pid, tid) = loop {
        if present_worker || syscall_counts {
            if let Ok(bytes) = fs::read("/tmp/legion-vk-present-thread.json")
                && let Some((pid, tid)) = marker_ids(&bytes)
                && let Ok(maps) = fs::read_to_string(format!("/proc/{pid}/maps"))
                && has_fixture_image(&maps)
                && std::path::Path::new(&format!("/proc/{pid}/task/{tid}")).is_dir()
            {
                break (pid, if syscall_counts { pid } else { tid });
            }
        } else if let Some(pid) = image_pid()? {
            break (pid, pid);
        }
        ensure!(
            Instant::now() < deadline,
            "owned workload thread did not appear"
        );
        match receive.recv_timeout(Duration::from_millis(10)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            _ => anyhow::bail!("workload ended before profiler attached"),
        }
    };
    let stat_path = format!("/proc/{pid}/stat");
    let identity = |stat: &str| {
        stat.rsplit_once(')')
            .and_then(|(_, r)| r.split_whitespace().nth(19))
            .map(str::to_owned)
    };
    let starttime =
        identity(&fs::read_to_string(&stat_path)?).context("missing workload start time")?;
    #[cfg(feature = "syscall-count-probe")]
    if let Target::QueuePlacement { colocate, server } = target {
        return syscall_probe::place_queues(receive, pid, &starttime, colocate, server);
    }
    #[cfg(feature = "syscall-count-probe")]
    if syscall_counts {
        return syscall_probe::collect(receive, pid, &starttime);
    }
    let mut ring = Ring::open(tid, present_worker)?;
    ensure!(
        identity(&fs::read_to_string(&stat_path)?).as_deref() == Some(starttime.as_str()),
        "profile target changed while attaching"
    );
    let counters = Counters::open(tid);
    let mut samples = Vec::with_capacity(MAX_SAMPLES);
    let mut mappings = Vec::new();
    let mut markers = Vec::new();
    let environment: std::collections::BTreeMap<_, _> = [
        "/proc/mtrr",
        "/sys/devices/system/cpu/vulnerabilities/spectre_v2",
        "/sys/devices/system/cpu/vulnerabilities/retbleed",
        "/sys/kernel/mm/transparent_hugepage/enabled",
    ]
    .into_iter()
    .map(|path| {
        (
            path,
            fs::read_to_string(path).unwrap_or_else(|e| format!("unavailable: {e}")),
        )
    })
    .collect();
    let mut smaps = String::new();
    let mut last_maps = Instant::now() - Duration::from_secs(1);
    let mut log_length = 0;
    let mut lost = 0;
    #[cfg(feature = "driver-context-probe")]
    let mut context_probe = driver_context_probe::Probe::default();
    loop {
        #[cfg(feature = "driver-context-probe")]
        let previous_samples = samples.len();
        ring.drain(&mut samples, &mut lost)?;
        let now = now_ns()?;
        #[cfg(feature = "driver-context-probe")]
        if present_worker
            && context_probe.due(now)
            && identity(&fs::read_to_string(&stat_path).unwrap_or_default()).as_deref()
                == Some(starttime.as_str())
        {
            context_probe.observe(pid, tid, &samples[previous_samples..], now);
        }
        if let Ok(log) = fs::read_to_string("/frames/proton-workload.stdout")
            && log.len() >= log_length
        {
            let complete = log.rfind('\n').map_or(log_length, |i| i + 1);
            for line in log[log_length..complete].lines() {
                if let Some(marker) = line.strip_prefix("LEGION_CPU_PROFILE_") {
                    markers.push(serde_json::json!({"time_ns":now,"marker":marker,"counters":counters.read()}));
                }
            }
            log_length = complete;
        }
        if last_maps.elapsed() >= Duration::from_millis(100) {
            let Ok(stat) = fs::read_to_string(&stat_path) else {
                break;
            };
            ensure!(
                identity(&stat).as_deref() == Some(starttime.as_str()),
                "profile target PID was reused"
            );
            if let Ok(maps) = fs::read_to_string(format!("/proc/{pid}/maps")) {
                mappings.push(serde_json::json!({"time_ns":now,"maps":maps}));
            }
            if let Ok(value) = fs::read_to_string(format!("/proc/{pid}/smaps")) {
                smaps = value;
            }
            last_maps = Instant::now();
        }
        match receive.recv_timeout(Duration::from_millis(2)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            _ => break,
        }
    }
    ring.drain(&mut samples, &mut lost)?;
    ensure!(
        !samples.is_empty() && (present_worker || !markers.is_empty()),
        "CPU profile has no samples or measurement markers"
    );
    #[allow(unused_mut)]
    let mut report = serde_json::json!({"schema":1,"diagnostic_only":true,"pid":pid,"target_tid":tid,"present_worker":present_worker,"starttime":starttime,
                "event":"software cpu-clock userspace only","clock":"CLOCK_MONOTONIC","period_ns":500000,"register_mask":REG_MASK,
                "lost":lost,"samples":samples,"mappings":mappings,"markers":markers,
                "environment":environment,"last_smaps":smaps,
        "marker_uncertainty":"flushed stdout polled nominally every 2 ms; processing and scheduling can add delay; not exact instruction boundaries"});
    #[cfg(feature = "driver-context-probe")]
    if present_worker {
        report["driver_context_probe"] = context_probe.report();
    }
    fs::write("/frames/cpu-profile.json", serde_json::to_vec(&report)?)?;
    Ok(())
}

pub(super) struct CpuProfile {
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<Result<()>>>,
}
impl CpuProfile {
    pub(super) fn configure_guest_wakeups(direct: bool) -> Result<()> {
        #[cfg(feature = "syscall-count-probe")]
        {
            syscall_probe::configure_wakeups(direct)
        }
        #[cfg(not(feature = "syscall-count-probe"))]
        {
            let _ = direct;
            anyhow::bail!("guest wakeup control requires the development Cargo feature")
        }
    }
    pub(super) fn start() -> Result<Self> {
        Self::start_target(Target::Main)
    }
    pub(super) fn start_present_worker() -> Result<Self> {
        Self::start_target(Target::Present)
    }
    pub(super) fn start_syscalls() -> Result<Self> {
        ensure!(
            cfg!(feature = "syscall-count-probe"),
            "syscall counts require the development Cargo feature"
        );
        Self::start_target(Target::Syscalls)
    }
    pub(super) fn start_queue_placement(colocate: bool, server: bool) -> Result<Self> {
        ensure!(
            cfg!(feature = "syscall-count-probe"),
            "queue placement requires the development Cargo feature"
        );
        Self::start_target(Target::QueuePlacement { colocate, server })
    }
    fn start_target(target: Target) -> Result<Self> {
        let init = fs::read_link("/proc/1/exe")?;
        let init_args = fs::read("/proc/1/cmdline")?;
        let args: Vec<_> = init_args.split(|b| *b == 0).collect();
        let private_native = init == std::path::Path::new("/usr/bin/bwrap")
            && args.contains(&b"--unshare-pid".as_slice())
            && args.contains(&b"/usr/share/legion/bootstrap".as_slice());
        if matches!(target, Target::Syscalls | Target::QueuePlacement { .. }) {
            ensure!(
                init == std::path::Path::new("/init")
                    && fs::read_to_string("/proc/cmdline")?.contains("legion.vulkan=1"),
                "syscall counter requires the owned disposable guest"
            );
        }
        ensure!(
            private_native
                || init == std::path::Path::new("/usr/share/legion/bootstrap")
                || (init == std::path::Path::new("/init")
                    && fs::read_to_string("/proc/cmdline")?.contains("legion.vulkan=1")),
            "CPU profiler requires an owned fixture PID namespace"
        );
        let (stop, receive) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("owned-cpu-profile".into())
            .spawn(move || collect_profile(receive, target))?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
    pub(super) fn finish(mut self) -> Result<()> {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("CPU sampler panicked"))??;
        }
        Ok(())
    }
}
impl Drop for CpuProfile {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_marker_needs_positive_ids_and_the_exact_fixture_mapping() {
        assert_eq!(marker_ids(br#"{"schema":1,"pid":7,"tid":9}"#), Some((7, 9)));
        for bytes in [
            br#"{"schema":1,"pid":0,"tid":9}"#.as_slice(),
            br#"{"schema":2,"pid":7,"tid":9}"#,
            b"{",
            br#"{"schema":1,"pid":7,"tid":-2}"#,
        ] {
            assert_eq!(marker_ids(bytes), None);
        }
        assert!(has_fixture_image(
            "1000-2000 r--p 0 00:00 1 /usr/share/legion/workload.exe"
        ));
        assert!(!has_fixture_image(
            "1000-2000 r--p 0 00:00 1 /other/workload.exe"
        ));
    }

    #[test]
    fn user_stack_captures_only_valid_bytes_and_rejects_bad_extents() {
        let mut bytes = vec![0; 40];
        bytes.extend_from_slice(&16_u64.to_ne_bytes());
        bytes.extend_from_slice(&0x1234_u64.to_ne_bytes());
        bytes.extend_from_slice(&0x5678_u64.to_ne_bytes());
        bytes.extend_from_slice(&8_u64.to_ne_bytes());
        assert_eq!(
            decode_sample_with_stack(&bytes, true)
                .expect("bounded stack")
                .stack_words,
            [0x1234]
        );
        bytes[64..72].copy_from_slice(&17_u64.to_ne_bytes());
        assert!(decode_sample_with_stack(&bytes, true).is_err());
        bytes[40..48].copy_from_slice(&u64::from(STACK_BYTES + 8).to_ne_bytes());
        assert!(decode_sample_with_stack(&bytes, true).is_err());
        assert!(decode_sample_with_stack(&[0; 40], true).is_err());
    }
    #[test]
    fn sampling_clock_matches_markers_and_does_not_inherit_or_include_kernel_work() {
        let attr = sampling_attribute();
        assert_eq!(attr.kind, 1);
        assert_eq!(attr.clock, libc::CLOCK_MONOTONIC);
        assert_ne!(attr.flags & (1 << 25), 0);
        assert_eq!(attr.flags & (1 << 1), 0);
        assert_eq!(attr.flags & ((1 << 5) | (1 << 6)), (1 << 5) | (1 << 6));
        assert_eq!(attr.regs, REG_MASK);
    }
    #[test]
    fn wrapped_records_and_short_samples_are_checked_without_reading_past_the_ring() {
        assert_eq!(ring_segments(8, 6, 4).expect("wrapped"), (6, 2));
        assert!(ring_segments(8, 0, 9).is_err());
        assert!(decode_sample(&[0; 32]).is_err());
        let mut sample = vec![0; 40];
        sample[8..16].copy_from_slice(&99_u64.to_ne_bytes());
        assert_eq!(decode_sample(&sample).expect("no-register ABI").ip, 99);
        sample[32..40].copy_from_slice(&2_u64.to_ne_bytes());
        assert!(decode_sample(&sample).is_err());
        sample.resize(40 + REG_MASK.count_ones() as usize * 8, 0);
        assert_eq!(
            decode_sample(&sample).expect("full registers").regs.len(),
            18
        );
        sample[32..40].copy_from_slice(&1_u64.to_ne_bytes());
        assert_eq!(
            decode_sample(&sample)
                .expect("32-bit transition registers")
                .abi,
            1
        );
    }
}
