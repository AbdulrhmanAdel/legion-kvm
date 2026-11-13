use super::syscall_probe;
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{FileExt, OpenOptionsExt},
    },
    sync::mpsc,
};

const PATH: &str = "/frames/queue-latency.bin";
const BYTES: u64 = 64 + 8192 * 32;
const MAGIC: &[u8; 8] = b"LQLAT001";

pub(super) fn prepare(opcode: u32, stride: u32, uid: u32, gid: u32) -> Result<()> {
    syscall_probe::guest_only()?;
    ensure!(
        (1..=4096).contains(&stride),
        "invalid queue sampling stride"
    );
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(PATH)?;
    file.set_len(BYTES)?;
    let mut header = [0; 64];
    header[..8].copy_from_slice(MAGIC);
    header[16..20].copy_from_slice(&opcode.to_le_bytes());
    header[20..24].copy_from_slice(&stride.to_le_bytes());
    file.write_all(&header)?;
    // SAFETY: ownership changes only on this newly created private profiling descriptor.
    let status = unsafe { libc::fchown(file.as_raw_fd(), uid, gid) };
    ensure!(
        status == 0,
        "set queue profiling owner: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

pub(super) fn collect(receive: mpsc::Receiver<()>, pid: i32, tid: i32) -> Result<()> {
    syscall_probe::guest_only()?;
    let mut file = fs::OpenOptions::new().read(true).write(true).open(PATH)?;
    ensure!(
        file.metadata()?.len() == BYTES,
        "queue control extent differs"
    );
    let mut header = [0; 64];
    file.read_exact(&mut header)?;
    ensure!(&header[..8] == MAGIC, "queue control magic differs");
    ensure!(pid > 0 && tid > 0, "invalid queue identity");
    file.write_all_at(&(pid as u32).to_le_bytes(), 8)?;
    file.write_all_at(&(tid as u32).to_le_bytes(), 12)?;
    fs::write(
        "/frames/queue-latency-target.json",
        serde_json::to_vec_pretty(&serde_json::json!({"pid":pid,"tid":tid,"guest_only":true}))?,
    )?;
    receive
        .recv()
        .context("queue sampler stop channel closed")?;
    file.read_exact_at(&mut header, 0)?;
    let value = |offset| {
        u64::from_le_bytes(
            header[offset..offset + 8]
                .try_into()
                .expect("fixed header word"),
        )
    };
    ensure!(
        value(24) >= 10 && value(24) <= value(40),
        "queue sampler did not complete enough requests"
    );
    ensure!(
        value(48) == 0 && value(56) == 0,
        "queue sampler dropped or overflowed records"
    );
    Ok(())
}
