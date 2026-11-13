//! What this host's NVIDIA driver looks like, as the guest needs to be told.

use crate::virtio::GpuSlot;
use std::path::Path;

/// Where a loaded NVIDIA kernel module publishes itself.
pub const PROC_NVIDIA: &str = "/proc/driver/nvidia";

/// The loaded driver's version, e.g.
pub fn driver_version(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("version")).ok()?;
    // "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64 615.71.09 ..." Matched by shape rather than by position: the words around it differ between the open and proprietary modules, and have changed before.
    text.split_whitespace()
        .find(|w| {
            let mut parts = w.split('.');
            let ok = |p: Option<&str>| {
                p.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            };
            ok(parts.next()) && ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
        })
        .map(str::to_string)
}

/// Every GPU the host driver owns, in PCI address order.
pub fn gpu_slots(root: &Path) -> Vec<GpuSlot> {
    let Ok(entries) = std::fs::read_dir(root.join("gpus")) else {
        return Vec::new();
    };
    let mut addrs: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    addrs.sort();

    addrs
        .iter()
        .enumerate()
        .map(|(minor, addr)| {
            let info = std::fs::read_to_string(root.join("gpus").join(addr).join("information"))
                .unwrap_or_default();
            // The index is the minor only because the kernel numbers /dev/nvidiaN in the same PCI order it lists these directories in.
            GpuSlot::new(addr, minor as u32, &info)
        })
        .collect()
}

#[cfg(test)]
#[path = "../tests/unit/host.rs"]
mod tests;
