mod vulkan;

use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use legion_display_channel::{Channel, FrameKind, FrameMeta};
use serde_json::json;

fn main() {
    if let Err(error) = run() {
        println!(
            "{}",
            json!({"passed": false, "error": format!("{error:#}")})
        );
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let (file, len, pci_path) = display_bar()?;
    let wc_path = pci_path.join("resource2_wc");
    let write_combining = wc_path.exists();
    // SAFETY: the device was selected by its ivshmem PCI identity in this owning VM, and its actual BAR size bounds the map.
    let channel = if write_combining {
        let pixels = OpenOptions::new().read(true).write(true).open(wc_path)?;
        // SAFETY: the two descriptors identify this same owned BAR.
        unsafe { Channel::attach_bar_wc(file, pixels, len)? }
    } else {
        // SAFETY: the selected BAR has the checked extent and owned lifetime described above; every peer follows the shared channel protocol.
        unsafe { Channel::attach_bar(file, len)? }
    };
    let (width, height) = channel.dimensions();
    let mut producer = channel.producer()?;
    let mut gpu = vulkan::Gpu::new(width, height)?;
    let start = Instant::now();
    let mut frames = 0_u64;
    let mut dropped = 0_u64;
    let interval = Duration::from_nanos(1_000_000_000 / 60);
    let mut deadline = Instant::now();
    while !channel.stopped() && start.elapsed() < Duration::from_secs(90) {
        let input = channel.input();
        frames += 1;
        let color = input.rgba(frames);
        let before = Instant::now();
        gpu.render(color)?;
        let gpu_ns = u64::try_from(before.elapsed().as_nanos())?;
        if !producer.publish(
            gpu.pixels(),
            FrameMeta {
                kind: FrameKind::InputColor,
                number: frames,
                input,
                rgba: color,
                gpu_ns,
                copy_ns: 0,
            },
        )? {
            dropped += 1;
        }
        deadline += interval;
        if let Some(wait) = deadline.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        } else {
            deadline = Instant::now();
        }
    }
    ensure!(frames != 0, "display producer generated no frames");
    println!(
        "{}",
        json!({"passed": true, "device": gpu.name(), "frames_rendered": frames, "frames_dropped": dropped, "elapsed_ms": start.elapsed().as_millis(), "width": width, "height": height, "pci_device": pci_path, "transport": "ivshmem_rgba_readback", "write_combining": write_combining, "stopped_by_host": channel.stopped(), "zero_copy": false})
    );
    Ok(())
}

fn display_bar() -> Result<(std::fs::File, usize, PathBuf)> {
    for entry in fs::read_dir("/sys/bus/pci/devices")? {
        let path = entry?.path();
        if fs::read_to_string(path.join("vendor"))?.trim() != "0x1af4"
            || fs::read_to_string(path.join("device"))?.trim() != "0x1110"
        {
            continue;
        }
        fs::write(path.join("enable"), b"1").context("enable display ivshmem PCI device")?;
        let resources = fs::read_to_string(path.join("resource"))?;
        let line = resources
            .lines()
            .nth(2)
            .context("display device has no BAR2")?;
        let values = line
            .split_whitespace()
            .map(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            values.len() == 3 && values[1] >= values[0] && values[0] != 0,
            "display BAR is not assigned"
        );
        let len = usize::try_from(
            values[1]
                .checked_sub(values[0])
                .and_then(|n| n.checked_add(1))
                .context("invalid BAR size")?,
        )?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.join("resource2"))?;
        ensure!(file.as_raw_fd() >= 0, "invalid BAR descriptor");
        return Ok((file, len, path));
    }
    anyhow::bail!("no Legion ivshmem display device");
}
