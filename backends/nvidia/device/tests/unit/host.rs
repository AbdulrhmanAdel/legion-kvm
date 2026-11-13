use super::*;
use crate::virtio::{MAX_GPUS, VirtioGpuNvConfig};
use std::path::PathBuf;

struct Fixture(PathBuf);

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let base = std::env::temp_dir().join(format!(
            "nvgpu-host-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir");
        for (path, body) in files {
            let p = base.join(path);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(&p, body).expect("write");
        }
        Self(base)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The exact text an open 615.71.09 module publishes.
const OPEN_615: &str = "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  615.71.09  Release Build  (root@)  \nGCC version:  gcc version 16.2.1 20260810 (GCC)\n";

#[test]
fn reads_the_version_from_a_real_proc_line() {
    let f = Fixture::new(&[("version", OPEN_615)]);
    assert_eq!(driver_version(f.path()).as_deref(), Some("615.71.09"));
}

/// The proprietary module words the same line differently.
#[test]
fn reads_the_version_from_the_proprietary_wording_too() {
    let f = Fixture::new(&[(
        "version",
        "NVRM version: NVIDIA UNIX x86_64 Kernel Module  580.178.04  Tue Jul  7 12:18:12 UTC 2026\n",
    )]);
    assert_eq!(driver_version(f.path()).as_deref(), Some("580.178.04"));
}

#[test]
fn no_driver_loaded_is_none_not_an_error() {
    let f = Fixture::new(&[]);
    assert_eq!(driver_version(f.path()), None);
    assert!(gpu_slots(f.path()).is_empty());
}

#[test]
fn a_single_gpu_becomes_a_slot_the_driver_can_read() {
    let f = Fixture::new(&[(
        "gpus/0000:01:00.0/information",
        "Model: \t\t NVIDIA RTX A2000\nIRQ:   \t\t 152\n",
    )]);
    let slots = gpu_slots(f.path());
    assert_eq!(slots.len(), 1);
    assert_eq!(&slots[0].pci_addr[..12], b"0000:01:00.0");
    let minor = slots[0].minor;
    assert_eq!(minor, 0);
    let len = slots[0].info_len;
    assert!(len > 0, "the information text was not carried");
}

/// readdir order is not sorted, and a guest that sees GPUs in a different order across reboots will address the wrong card.
#[test]
fn gpus_are_ordered_by_pci_address() {
    let f = Fixture::new(&[
        ("gpus/0000:41:00.0/information", "Model: C\n"),
        ("gpus/0000:01:00.0/information", "Model: A\n"),
        ("gpus/0000:21:00.0/information", "Model: B\n"),
    ]);
    let slots = gpu_slots(f.path());
    let addrs: Vec<_> = slots
        .iter()
        .map(|s| String::from_utf8_lossy(&s.pci_addr[..12]).into_owned())
        .collect();
    assert_eq!(addrs, ["0000:01:00.0", "0000:21:00.0", "0000:41:00.0"]);
    let minors: Vec<u32> = slots.iter().map(|s| s.minor).collect();
    assert_eq!(minors, [0, 1, 2]);
}

/// An information file longer than the window must not be lost entirely, and must not be claimed as longer than it is.
#[test]
fn an_over_long_information_file_is_truncated_honestly() {
    let big = "Model: X\n".repeat(500);
    let f = Fixture::new(&[("gpus/0000:01:00.0/information", big.as_str())]);
    let slots = gpu_slots(f.path());
    let len = slots[0].info_len as usize;
    assert_eq!(len, crate::virtio::INFO_TEXT_LEN);
    assert_eq!(&slots[0].info_text[..8], b"Model: X");
}

/// The end the whole module exists for: a config a guest driver accepts.
#[test]
fn discovery_produces_a_config_the_driver_would_accept() {
    let f = Fixture::new(&[
        ("version", OPEN_615),
        ("gpus/0000:01:00.0/information", "Model: NVIDIA RTX A2000\n"),
    ]);
    let version = driver_version(f.path()).expect("a version");
    let cfg = VirtioGpuNvConfig::new(
        &version,
        &gpu_slots(f.path()),
        crate::caps::Caps::DEFAULT,
        0,
    );
    let n = cfg.num_gpus;
    assert!(
        n >= 1 && n as usize <= MAX_GPUS,
        "driver rejects num_gpus={n}"
    );
    assert_eq!(&cfg.as_bytes()[0..9], b"615.71.09");
}
