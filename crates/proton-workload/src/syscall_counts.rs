use super::{Attr, Ring, has_fixture_image, now_ns};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    os::fd::FromRawFd,
    os::unix::fs::MetadataExt,
    sync::mpsc,
    time::{Duration, Instant},
};

const EVENTS: [&str; 12] = [
    "syscalls/sys_enter_read",
    "syscalls/sys_enter_write",
    "syscalls/sys_enter_membarrier",
    "syscalls/sys_enter_mprotect",
    "syscalls/sys_enter_futex",
    "syscalls/sys_enter_futex_waitv",
    "syscalls/sys_enter_ioctl",
    "syscalls/sys_enter_mmap",
    "syscalls/sys_enter_munmap",
    "ipi/ipi_send_cpu",
    "ipi/ipi_send_cpumask",
    "sched/sched_waking",
];
const MAX_COUNTERS: usize = 512;
const IPI_STRIDE: u64 = 128;

fn cpu_list(text: &str) -> Result<BTreeSet<usize>> {
    let mut cpus = BTreeSet::new();
    for part in text.trim().split(',') {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let (a, b) = (a.parse::<usize>()?, b.parse::<usize>()?);
        ensure!(a <= b && b < 4096, "unsupported affinity extent");
        cpus.extend(a..=b);
    }
    ensure!(!cpus.is_empty(), "empty affinity");
    Ok(cpus)
}
fn affinity(path: &std::path::Path) -> Result<BTreeSet<usize>> {
    let status = fs::read_to_string(path.join("status"))?;
    cpu_list(
        status
            .lines()
            .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
            .context("missing task affinity")?,
    )
}
fn mask(cpu: usize) -> Result<Vec<usize>> {
    ensure!(cpu < 4096, "CPU outside diagnostic bound");
    let bits = usize::BITS as usize;
    let mut words = vec![0; cpu / bits + 1];
    words[cpu / bits] = 1usize << (cpu % bits);
    Ok(words)
}

fn private_prefix(environment: &[u8]) -> bool {
    let mut prefixes = environment
        .split(|b| *b == 0)
        .filter_map(|s| s.strip_prefix(b"WINEPREFIX="));
    prefixes
        .next()
        .is_some_and(|p| p == b"/frames/compatdata/pfx" || p == b"/frames/compatdata/pfx/")
        && prefixes.next().is_none()
}

pub(super) fn private_server() -> Result<Option<std::path::PathBuf>> {
    let launch: Value = serde_json::from_slice(&fs::read("/usr/share/legion/launch.json")?)?;
    let path = std::path::Path::new(
        launch["proton_path"]
            .as_str()
            .context("missing selected Proton")?,
    );
    ensure!(path.is_absolute(), "selected Proton path is not absolute");
    let expected = fs::metadata(path.join("files/bin/wineserver"))?;
    let uid = launch["uid"].as_u64().context("missing fixture UID")?;
    let mut found = None;
    let mut candidates = Vec::new();
    for process in fs::read_dir("/proc")? {
        let process = process?;
        if process
            .file_name()
            .to_string_lossy()
            .parse::<i32>()
            .is_err()
        {
            continue;
        }
        let Ok(exe) = fs::metadata(process.path().join("exe")) else {
            continue;
        };
        if fs::read_to_string(process.path().join("comm"))
            .is_ok_and(|comm| comm.trim() == "wineserver")
        {
            let environment = fs::read(process.path().join("environ"));
            let prefix = environment
                .as_ref()
                .ok()
                .and_then(|bytes| {
                    bytes
                        .split(|b| *b == 0)
                        .find_map(|s| s.strip_prefix(b"WINEPREFIX="))
                })
                .map(|s| String::from_utf8_lossy(s).into_owned());
            candidates.push(json!({"pid":process.file_name().to_string_lossy(),
                "exe":fs::read_link(process.path().join("exe")).ok().map(|p|p.display().to_string()),"dev":exe.dev(),"ino":exe.ino(),
                "prefix":prefix,"environ_error":environment.err().map(|e|e.to_string())}));
        }
        if exe.dev() != expected.dev() || exe.ino() != expected.ino() {
            continue;
        }
        let Ok(environment) = fs::read(process.path().join("environ")) else {
            continue;
        };
        if !private_prefix(&environment) {
            continue;
        }
        let status = fs::read_to_string(process.path().join("status"))?;
        let actual_uid = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|line| line.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok());
        ensure!(
            actual_uid == Some(uid),
            "private wineserver UID differs from fixture"
        );
        ensure!(found.is_none(), "ambiguous private wineserver");
        found = Some(process.path());
    }
    fs::write(
        "/frames/server-selection.json",
        serde_json::to_vec_pretty(&json!({
            "expected_path":path.join("files/bin/wineserver").display().to_string(),"expected_dev":expected.dev(),"expected_ino":expected.ino(),
            "expected_uid":uid,"candidates":candidates,"selected":found.as_ref().map(|p|p.display().to_string())}))?,
    )?;
    Ok(found)
}

pub(super) fn place_queues(
    receive: mpsc::Receiver<()>,
    pid: i32,
    starttime: &str,
    colocate: bool,
    server: bool,
) -> Result<()> {
    guest_only()?;
    let deadline = Instant::now() + Duration::from_secs(20);
    let pid_stat = format!("/proc/{pid}/stat");
    let mut report = None;
    while let Ok(stat) = fs::read_to_string(&pid_stat) {
        ensure!(
            identity(&stat) == Some(starttime),
            "fixture process reused during placement"
        );
        if report.is_none() {
            let mut pair = BTreeMap::new();
            for task in fs::read_dir(format!("/proc/{pid}/task"))? {
                let task = task?;
                let Ok(comm) = fs::read_to_string(task.path().join("comm")) else {
                    continue;
                };
                let comm = comm.trim();
                if comm == "vkd3d_queue" || (!server && comm == "vkd3d-swapchain") {
                    ensure!(
                        pair.insert(comm.to_owned(), task.path()).is_none(),
                        "ambiguous owned queue workers"
                    );
                }
            }
            if server && let Some(path) = private_server()? {
                pair.insert("wineserver".to_owned(), path);
            }
            if pair.len() == 2 {
                let queue = &pair["vkd3d_queue"];
                let stat = fs::read_to_string(queue.join("stat"))?;
                let current = stat
                    .rsplit_once(')')
                    .context("queue stat missing comm")?
                    .1
                    .split_whitespace()
                    .nth(36)
                    .context("queue CPU missing")?
                    .parse::<usize>()?;
                let allowed: Vec<_> = pair
                    .values()
                    .map(|path| affinity(path))
                    .collect::<Result<_>>()?;
                let common: BTreeSet<_> = allowed[0].intersection(&allowed[1]).copied().collect();
                let cpu = if common.contains(&current) {
                    current
                } else {
                    *common
                        .first()
                        .context("workers have no common allowed CPU")?
                };
                let words = mask(cpu)?;
                let mut workers = Vec::new();
                for (name, path) in pair {
                    let tid = path
                        .file_name()
                        .context("missing TID")?
                        .to_string_lossy()
                        .parse::<i32>()?;
                    ensure!(
                        tid > 0 && tid != pid,
                        "placement target is not a background worker"
                    );
                    let before = fs::read_to_string(path.join("stat"))?;
                    let thread_start = identity(&before).context("missing thread start")?;
                    let original = affinity(&path)?;
                    if name == "wineserver" {
                        ensure!(
                            private_server()?.as_ref() == Some(&path),
                            "private wineserver changed before placement"
                        );
                    }
                    ensure!(
                        fs::read_to_string(path.join("comm"))?.trim() == name,
                        "queue worker changed"
                    );
                    if colocate {
                        // SAFETY: validated owned guest TID, live aligned mask covering exactly its explicit byte length; no pointer is retained.
                        let result = unsafe {
                            libc::syscall(
                                libc::SYS_sched_setaffinity,
                                tid,
                                std::mem::size_of_val(words.as_slice()),
                                words.as_ptr(),
                            )
                        };
                        ensure!(
                            result == 0,
                            "guest worker affinity failed: {}",
                            std::io::Error::last_os_error()
                        );
                    }
                    ensure!(
                        fs::read_to_string(path.join("stat"))
                            .ok()
                            .as_deref()
                            .and_then(identity)
                            == Some(thread_start),
                        "worker reused during placement"
                    );
                    let after = affinity(&path)?;
                    ensure!(
                        after
                            == if colocate {
                                BTreeSet::from([cpu])
                            } else {
                                original.clone()
                            },
                        "worker placement did not match requested state"
                    );
                    workers.push(json!({"tid":tid,"comm":name,"starttime":thread_start,"before":original,"after":after}));
                }
                report = Some(
                    json!({"schema":1,"diagnostic_only":true,"guest_only":true,"pid":pid,"colocated":colocate,"server_pair":server,"cpu":cpu,"workers":workers}),
                );
                fs::write(
                    "/frames/queue-placement.json",
                    serde_json::to_vec_pretty(
                        report.as_ref().context("missing placement report")?,
                    )?,
                )?;
            } else {
                ensure!(
                    Instant::now() < deadline,
                    "owned queue workers did not appear for PID {pid}: server={server}, workers={:?}",
                    pair.keys().collect::<Vec<_>>()
                )
            }
        }
        match receive.recv_timeout(Duration::from_millis(100)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            _ => break,
        }
    }
    ensure!(report.is_some(), "queue placement was not established");
    Ok(())
}

pub(super) fn guest_only() -> Result<()> {
    ensure!(
        fs::read_link("/proc/1/exe")? == std::path::Path::new("/init")
            && fs::read_to_string("/proc/cmdline")?.contains("legion.vulkan=1"),
        "diagnostic requires the disposable owned guest"
    );
    Ok(())
}

fn scheduler_features(text: &str) -> Result<BTreeMap<&str, bool>> {
    let mut features = BTreeMap::new();
    for token in text.split_whitespace() {
        let (name, enabled) = token
            .strip_prefix("NO_")
            .map_or((token, true), |name| (name, false));
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|c| c.is_ascii_uppercase() || c == b'_' || c.is_ascii_digit()),
            "invalid scheduler feature token"
        );
        ensure!(
            features.insert(name, enabled).is_none(),
            "duplicate scheduler feature"
        );
    }
    ensure!(
        features.contains_key("TTWU_QUEUE"),
        "guest does not expose TTWU_QUEUE"
    );
    Ok(features)
}

pub(super) fn configure_wakeups(direct: bool) -> Result<()> {
    guest_only()?;
    let path = "/sys/kernel/debug/sched/features";
    if !std::path::Path::new(path).exists() {
        // SAFETY: all strings are NUL-terminated and live for the syscall; no mount data is passed. guest_only proved this is the owned VM kernel.
        let result = unsafe {
            libc::mount(
                c"debugfs".as_ptr(),
                c"/sys/kernel/debug".as_ptr(),
                c"debugfs".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                std::ptr::null(),
            )
        };
        ensure!(
            result == 0,
            "guest debugfs mount failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let before = fs::read_to_string(path)?;
    let mut expected = scheduler_features(&before)?;
    expected.insert("TTWU_QUEUE", !direct);
    fs::write(
        path,
        if direct {
            "NO_TTWU_QUEUE"
        } else {
            "TTWU_QUEUE"
        },
    )?;
    let after = fs::read_to_string(path)?;
    ensure!(
        scheduler_features(&after)? == expected,
        "guest wakeup feature did not match the single requested change"
    );
    fs::write(
        "/frames/guest-wakeup-control.json",
        serde_json::to_vec_pretty(&json!({
        "schema":1,"diagnostic_only":true,"guest_only":true,"direct":direct,
        "before":before,"after":after,"host_changed":false}))?,
    )?;
    Ok(())
}

fn trace_field(format: &str, name: &str, size: usize) -> Result<usize> {
    let line = format
        .lines()
        .find(|line| {
            line.split(';')
                .next()
                .is_some_and(|f| f.split_whitespace().last() == Some(name))
        })
        .context("trace field absent")?;
    let field = |key: &str| -> Option<usize> {
        line.split(';')
            .find_map(|part| part.trim().strip_prefix(key)?.trim().parse().ok())
    };
    ensure!(field("size:") == Some(size), "unexpected trace field size");
    field("offset:").context("trace field offset absent")
}
fn symbol(symbols: &[(u64, String)], address: u64) -> String {
    let index = symbols.partition_point(|(start, _)| *start <= address);
    match index.checked_sub(1).and_then(|i| symbols.get(i)) {
        Some((start, name)) => format!("{name}+{:#x}", address - start),
        None => format!("unresolved:{address:#x}"),
    }
}
fn word(bytes: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_ne_bytes(
        bytes
            .get(at..at.checked_add(8).context("word overflow")?)
            .context("short IPI word")?
            .try_into()?,
    ))
}
fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_ne_bytes(
        bytes
            .get(at..at.checked_add(4).context("field overflow")?)
            .context("short IPI field")?
            .try_into()?,
    ))
}
struct KernelSample<'a> {
    ip: u64,
    time_ns: u64,
    chain: &'a [u8],
    raw: &'a [u8],
}
fn kernel_sample(record: &[u8], pid: i32, tid: i32) -> Result<KernelSample<'_>> {
    ensure!(
        i32::try_from(u32_at(record, 16)?)? == pid && i32::try_from(u32_at(record, 20)?)? == tid,
        "kernel sample escaped verified task"
    );
    let nr = usize::try_from(word(record, 32)?)?;
    ensure!(nr <= 256, "IPI stack exceeds capture bound");
    let chain_end = 40 + nr * 8;
    let raw_size = usize::try_from(u32_at(record, chain_end)?)?;
    ensure!(raw_size <= 4096, "IPI raw extent exceeds bound");
    let raw = record
        .get(chain_end + 4..chain_end + 4 + raw_size)
        .context("truncated IPI raw data")?;
    let chain = record
        .get(40..chain_end)
        .context("truncated kernel stack")?;
    Ok(KernelSample {
        ip: word(record, 8)?,
        time_ns: word(record, 24)?,
        chain,
        raw,
    })
}
fn chain_symbols(sample: &KernelSample<'_>, symbols: &[(u64, String)]) -> Vec<String> {
    sample
        .chain
        .as_chunks::<8>()
        .0
        .iter()
        .map(|word| u64::from_ne_bytes(*word))
        .filter(|address| *address < u64::MAX - 4095)
        .map(|address| symbol(symbols, address))
        .collect()
}
fn ipi_sample(
    record: &[u8],
    pid: i32,
    tid: i32,
    fields: [usize; 3],
    symbols: &[(u64, String)],
) -> Result<Value> {
    let sample = kernel_sample(record, pid, tid)?;
    Ok(
        json!({"pid":pid,"tid":tid,"time_ns":sample.time_ns,"ip":symbol(symbols,sample.ip),
        "target_cpu":u32_at(sample.raw,fields[0])?,"callsite":symbol(symbols,word(sample.raw,fields[1])?),
        "callback":symbol(symbols,word(sample.raw,fields[2])?),"kernel_stack":chain_symbols(&sample,symbols)}),
    )
}

fn waking_sample(
    record: &[u8],
    pid: i32,
    tid: i32,
    fields: [usize; 3],
    symbols: &[(u64, String)],
) -> Result<Value> {
    let sample = kernel_sample(record, pid, tid)?;
    let comm = sample
        .raw
        .get(fields[0]..fields[0].checked_add(16).context("comm offset overflow")?)
        .context("short wakeup comm")?;
    let end = comm.iter().position(|b| *b == 0).unwrap_or(comm.len());
    let target_tid = i32::try_from(u32_at(sample.raw, fields[1])?)?;
    ensure!(target_tid > 0, "invalid wakeup target TID");
    Ok(json!({"pid":pid,"tid":tid,"time_ns":sample.time_ns,
        "target_tid":target_tid,"target_comm":std::str::from_utf8(&comm[..end])?,
        "target_cpu":u32_at(sample.raw,fields[2])?,"kernel_stack":chain_symbols(&sample,symbols)}))
}

fn ipi_attribute(event: u64) -> Attr {
    Attr {
        kind: 2,
        size: std::mem::size_of::<Attr>() as u32,
        config: event,
        period: IPI_STRIDE,
        sample: 1 | 2 | 4 | (1 << 5) | (1 << 10),
        flags: (1 << 4) | (1 << 6) | (1 << 22) | (1 << 25),
        clock: libc::CLOCK_MONOTONIC,
        wakeup: 1,
        ..Attr::default()
    }
}

fn identity(stat: &str) -> Option<&str> {
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)
}
fn values(bytes: [u8; 24]) -> Result<[u64; 3]> {
    let mut values = [0; 3];
    for (out, word) in values.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *out = u64::from_ne_bytes(*word);
    }
    ensure!(
        values[2] <= values[1],
        "counter running time exceeds enabled time"
    );
    Ok(values)
}
fn open(tid: i32, event: u64) -> Result<fs::File> {
    let attr = Attr {
        kind: 2,
        size: std::mem::size_of::<Attr>() as u32,
        config: event,
        read_format: 3,
        flags: (1 << 4) | (1 << 6),
        ..Attr::default()
    };
    // SAFETY: the kernel reads the initialized ABI attribute synchronously.
    let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, &attr, tid, -1, -1, 8_u64) };
    ensure!(
        fd >= 0,
        "tracepoint open failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: successful perf_event_open returned this newly owned descriptor.
    Ok(unsafe { fs::File::from_raw_fd(i32::try_from(fd)?) })
}

pub(super) fn collect(receive: mpsc::Receiver<()>, pid: i32, starttime: &str) -> Result<()> {
    guest_only()?;
    let pid_stat = format!("/proc/{pid}/stat");
    let task_dir = format!("/proc/{pid}/task");
    ensure!(
        has_fixture_image(&fs::read_to_string(format!("/proc/{pid}/maps"))?),
        "owned PE mapping disappeared"
    );
    // tracefs is mounted only in the verified guest kernel, without enabling tracing or writing event controls. perf opens below remain per-task.
    if !std::path::Path::new("/sys/kernel/tracing/events/syscalls").is_dir() {
        // SAFETY: these are live NUL-terminated strings; mount uses no data pointer.
        let result = unsafe {
            libc::mount(
                c"tracefs".as_ptr(),
                c"/sys/kernel/tracing".as_ptr(),
                c"tracefs".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                std::ptr::null(),
            )
        };
        ensure!(
            result == 0,
            "guest tracefs mount failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let events: Vec<_> = EVENTS
        .iter()
        .map(|name| {
            let id = fs::read_to_string(format!("/sys/kernel/tracing/events/{name}/id"))
                .map_err(|e| e.to_string())
                .and_then(|s| s.trim().parse::<u64>().map_err(|e| e.to_string()));
            (*name, id)
        })
        .collect();
    let started = now_ns()?;
    let format = fs::read_to_string("/sys/kernel/tracing/events/ipi/ipi_send_cpu/format")?;
    let fields = [
        trace_field(&format, "cpu", 4)?,
        trace_field(&format, "callsite", 8)?,
        trace_field(&format, "callback", 8)?,
    ];
    let wake_format = fs::read_to_string("/sys/kernel/tracing/events/sched/sched_waking/format")?;
    let wake_fields = [
        trace_field(&wake_format, "comm[16]", 16)?,
        trace_field(&wake_format, "pid", 4)?,
        trace_field(&wake_format, "target_cpu", 4)?,
    ];
    let mut symbols: Vec<_> = fs::read_to_string("/proc/kallsyms")?
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let address = u64::from_str_radix(parts.next()?, 16).ok()?;
            parts.next()?;
            let name = parts.next()?;
            (address != 0).then(|| (address, name.to_owned()))
        })
        .collect();
    symbols.sort_by_key(|(address, _)| *address);
    let ipi_id = events
        .iter()
        .find_map(|(name, id)| {
            (*name == "ipi/ipi_send_cpu")
                .then_some(id.as_ref().ok())
                .flatten()
        })
        .copied();
    let mut rings = BTreeMap::new();
    let wake_id = events
        .iter()
        .find_map(|(name, id)| {
            (*name == "sched/sched_waking")
                .then_some(id.as_ref().ok())
                .flatten()
        })
        .copied();
    let mut wake_rings = BTreeMap::new();
    let mut wake_records = Vec::new();
    let mut wake_lost = 0;
    let mut ipi_records = Vec::new();
    let mut ipi_lost = 0;
    let mut thread_names = BTreeMap::new();
    let mut counters = BTreeMap::new();
    let mut errors = Vec::new();
    let mut limited = false;
    while let Ok(stat) = fs::read_to_string(&pid_stat) {
        ensure!(
            identity(&stat) == Some(starttime),
            "owned fixture PID reused"
        );
        for task in fs::read_dir(&task_dir)? {
            let task = task?;
            let Ok(tid) = task.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let thread_stat = task.path().join("stat");
            let Ok(before) = fs::read_to_string(&thread_stat) else {
                continue;
            };
            let Some(thread_start) = identity(&before) else {
                continue;
            };
            let thread_key = (tid, thread_start.to_owned());
            if !rings.contains_key(&thread_key) && rings.len() < 64 {
                let ring = ipi_id
                    .context("IPI tracepoint unavailable")
                    .and_then(|id| Ring::from_attribute(tid, ipi_attribute(id), false))
                    .map_err(|e| format!("{e:#}"));
                let wake_ring = wake_id
                    .context("scheduler waking tracepoint unavailable")
                    .and_then(|id| Ring::from_attribute(tid, ipi_attribute(id), false))
                    .map_err(|e| format!("{e:#}"));
                if fs::read_to_string(&thread_stat)
                    .ok()
                    .as_deref()
                    .and_then(identity)
                    != Some(thread_start)
                {
                    continue;
                }
                thread_names.insert(
                    thread_key.clone(),
                    fs::read_to_string(task.path().join("comm")).unwrap_or_default(),
                );
                wake_rings.insert(thread_key.clone(), wake_ring);
                rings.insert(thread_key, ring);
            }
            for (name, id) in &events {
                let Ok(id) = id else { continue };
                let key = (tid, thread_start.to_owned(), *name);
                if counters.contains_key(&key) {
                    continue;
                }
                if counters.len() >= MAX_COUNTERS {
                    limited = true;
                    break;
                }
                let result = open(tid, *id).map_err(|e| format!("{e:#}"));
                if fs::read_to_string(&thread_stat)
                    .ok()
                    .as_deref()
                    .and_then(identity)
                    != Some(thread_start)
                {
                    continue;
                }
                if let Err(error) = &result {
                    errors.push(json!({"tid":tid,"event":name,"error":error}))
                }
                counters.insert(key, result);
            }
        }
        for ((tid, _), ring) in &mut rings {
            if let Ok(ring) = ring {
                ring.drain_records(
                    |record| {
                        ensure!(ipi_records.len() < 20_000, "IPI record cap reached");
                        ipi_records.push(ipi_sample(record, pid, *tid, fields, &symbols)?);
                        Ok(())
                    },
                    &mut ipi_lost,
                )?;
            }
        }
        for ((tid, _), ring) in &mut wake_rings {
            if let Ok(ring) = ring {
                ring.drain_records(
                    |record| {
                        ensure!(wake_records.len() < 20_000, "wakeup record cap reached");
                        wake_records.push(waking_sample(record, pid, *tid, wake_fields, &symbols)?);
                        Ok(())
                    },
                    &mut wake_lost,
                )?;
            }
        }
        match receive.recv_timeout(Duration::from_millis(100)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            _ => break,
        }
    }
    let mut ipi_errors = Vec::new();
    for ((tid, _), ring) in &mut rings {
        match ring {
            Ok(ring) => ring.drain_records(
                |record| {
                    ensure!(ipi_records.len() < 20_000, "IPI record cap reached");
                    ipi_records.push(ipi_sample(record, pid, *tid, fields, &symbols)?);
                    Ok(())
                },
                &mut ipi_lost,
            )?,
            Err(error) => ipi_errors.push(json!({"tid":tid,"error":error})),
        }
    }
    let mut counts = Vec::new();
    let mut wake_errors = Vec::new();
    for ((tid, _), ring) in &mut wake_rings {
        match ring {
            Ok(ring) => ring.drain_records(
                |record| {
                    ensure!(wake_records.len() < 20_000, "wakeup record cap reached");
                    wake_records.push(waking_sample(record, pid, *tid, wake_fields, &symbols)?);
                    Ok(())
                },
                &mut wake_lost,
            )?,
            Err(error) => wake_errors.push(json!({"tid":tid,"error":error})),
        }
    }
    for ((tid, thread_start, event), file) in counters {
        let value = file.and_then(|mut file| {
            let mut bytes = [0; 24];
            file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
            values(bytes).map_err(|e| format!("{e:#}"))
        });
        counts.push(match value {
            Ok([value, enabled_ns, running_ns]) => json!({"tid":tid,"thread_start":thread_start,
                "event":event,"value":value,"enabled_ns":enabled_ns,"running_ns":running_ns}),
            Err(error) => json!({"tid":tid,"event":event,"unavailable":error}),
        });
    }
    let report = json!({"schema":1,"diagnostic_only":true,"pid":pid,"starttime":starttime,
        "started_ns":started,"ended_ns":now_ns()?,"limited":limited,"counts":counts,"errors":errors,
        "ipi_stride":IPI_STRIDE,"ipi_samples":ipi_records,"ipi_lost":ipi_lost,"ipi_errors":ipi_errors,
        "ipi_format":format,"kernel_symbols_available":!symbols.is_empty(),
        "wakeup_format":wake_format,"wakeup_stride":IPI_STRIDE,"wakeup_samples":wake_records,
        "wakeup_lost":wake_lost,"wakeup_errors":wake_errors,
        "threads":thread_names.into_iter().map(|((tid,starttime),name)|json!({"tid":tid,"starttime":starttime,"comm":name.trim()})).collect::<Vec<_>>(),
        "events":events.into_iter().map(|(name,id)|match id {Ok(id)=>json!({"name":name,"id":id}),
            Err(error)=>json!({"name":name,"unavailable":error})}).collect::<Vec<_>>(),
        "uncertainty":"threads attached after owned executable discovery; new threads polled every 100ms; exited short-lived threads can be missed; no syscall timing or arguments captured"});
    fs::write(
        "/frames/owned-syscall-counts.json",
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/syscall_counts.rs"]
mod tests;
