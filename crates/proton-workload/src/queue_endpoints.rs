use super::{has_fixture_image, now_ns, syscall_probe};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path, sync::mpsc, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IoCall {
    operation: &'static str,
    fd: u32,
    requested_bytes: u64,
}

fn io_call(text: &str) -> Option<IoCall> {
    let fields: Vec<_> = text.split_whitespace().collect();
    if fields.len() != 9 {
        return None;
    }
    let operation = match fields[0] {
        "0" => "read",
        "1" => "write",
        _ => return None,
    };
    let number = |text: &str| {
        text.strip_prefix("0x")
            .and_then(|digits| u64::from_str_radix(digits, 16).ok())
    };
    Some(IoCall {
        operation,
        fd: u32::try_from(number(fields[1])?).ok()?,
        requested_bytes: number(fields[3])?,
    })
}

fn descriptors(process: &Path) -> Result<Value> {
    let mut descriptors = Vec::new();
    for entry in fs::read_dir(process.join("fd"))?.take(8192) {
        let entry = entry?;
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        descriptors.push(json!({"fd":fd,"target":target.to_string_lossy(),
            "info":fs::read_to_string(process.join("fdinfo").join(fd.to_string())).ok()}));
    }
    Ok(
        json!({"pid":process.file_name().map(|p|p.to_string_lossy()),
        "comm":fs::read_to_string(process.join("comm")).ok(),
        "stat":fs::read_to_string(process.join("stat")).ok(),"fds":descriptors}),
    )
}

pub(super) fn collect(receive: mpsc::Receiver<()>, pid: i32, tid: i32, start: &str) -> Result<()> {
    syscall_probe::guest_only()?;
    let process = std::path::PathBuf::from(format!("/proc/{pid}"));
    ensure!(
        has_fixture_image(&fs::read_to_string(process.join("maps"))?),
        "endpoint target is not the owned fixture"
    );
    let server = syscall_probe::private_server()?.context("owned wineserver is unavailable")?;
    let tasks = process.join("task");
    let queue = tasks.join(tid.to_string());
    let started = now_ns()?;
    let mut snapshots = Vec::new();
    let mut observations = BTreeMap::new();
    let mut other_threads = BTreeMap::new();
    let mut last_snapshot = 0;
    let mut last_threads = 0;
    let mut samples = 0_u64;
    loop {
        let now = now_ns()?;
        let Ok(raw) = fs::read_to_string(queue.join("syscall")) else {
            break;
        };
        samples += 1;
        if let Some(call) = io_call(&raw) {
            let target = fs::read_link(process.join("fd").join(call.fd.to_string()))
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "closed".into());
            *observations
                .entry((call.operation, call.fd, target, call.requested_bytes))
                .or_insert(0_u64) += 1;
        }
        if now.saturating_sub(last_threads) >= 10_000_000 {
            for task in fs::read_dir(&tasks)?.take(128) {
                let task = task?;
                let Ok(raw) = fs::read_to_string(task.path().join("syscall")) else {
                    continue;
                };
                let Some(call) = io_call(&raw) else { continue };
                let comm = fs::read_to_string(task.path().join("comm")).unwrap_or_default();
                let target = fs::read_link(process.join("fd").join(call.fd.to_string()))
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "closed".into());
                *other_threads
                    .entry((
                        task.file_name().to_string_lossy().into_owned(),
                        comm.trim().to_owned(),
                        call.operation,
                        call.fd,
                        target,
                    ))
                    .or_insert(0_u64) += 1;
            }
            last_threads = now;
        }
        if now.saturating_sub(last_snapshot) >= 1_000_000_000 {
            let stat = fs::read_to_string(process.join("stat"))?;
            ensure!(
                stat.rsplit_once(')')
                    .and_then(|(_, s)| s.split_whitespace().nth(19))
                    == Some(start),
                "owned endpoint PID was reused"
            );
            snapshots.push(
                json!({"time_ns":now,"peers":[descriptors(&process)?,descriptors(&server)?]}),
            );
            ensure!(
                snapshots.len() <= 120,
                "endpoint profile exceeded two minutes"
            );
            last_snapshot = now;
        }
        match receive.recv_timeout(Duration::from_millis(1)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            _ => break,
        }
    }
    let observations: Vec<_> = observations.into_iter().map(|((operation,fd,target,requested_bytes),count)|json!({"operation":operation,"fd":fd,"target":target,"requested_bytes":requested_bytes,"observations":count})).collect();
    let other_threads: Vec<_> = other_threads.into_iter().map(|((tid,comm,operation,fd,target),count)|json!({"tid":tid,"comm":comm,"operation":operation,"fd":fd,"target":target,"observations":count})).collect();
    fs::write(
        "/frames/queue-endpoints.json",
        serde_json::to_vec_pretty(
            &json!({"schema":1,"pid":pid,"tid":tid,"started_ns":started,"ended_ns":now_ns()?,"samples":samples,
        "observations":observations,"snapshots":snapshots,"other_threads":other_threads,
        "scope":"read-only syscall-state sampling; observations are not call counts; descriptor snapshots are not atomic"}),
        )?,
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/queue_endpoints.rs"]
mod tests;
