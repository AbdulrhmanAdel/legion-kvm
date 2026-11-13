use super::*;
use std::mem::{offset_of, size_of};

/// The driver asserts these exact numbers.
#[test]
fn layout_matches_the_guest_driver() {
    assert_eq!(size_of::<GpuSlot>(), 476, "gpu_slot size mismatch");
    assert_eq!(size_of::<VirtioGpuNvConfig>(), 4024, "config size mismatch");
    // Where the guest driver's struct ends; it reads nothing past here.
    assert_eq!(offset_of!(VirtioGpuNvConfig, vram_limit_mib), 4016);
    assert_eq!(
        offset_of!(VirtioGpuNvConfig, num_fd_translations),
        3880,
        "fd_translations offset mismatch"
    );
}

/// The constraint behind every number above: a guest maps device config with PAGE_SIZE as its maximum and truncates the rest without saying so, so anything past 4096 is not slow or wasteful -- it is unreadable, and reading it takes the guest driver down.
#[test]
fn config_fits_in_one_page() {
    assert!(
        size_of::<VirtioGpuNvConfig>() <= 4096,
        "config is {} bytes; a guest cannot read past 4096",
        size_of::<VirtioGpuNvConfig>()
    );
}

/// Every field the driver reads by a fixed offset.
#[test]
fn field_offsets_are_where_the_driver_reads_them() {
    assert_eq!(offset_of!(VirtioGpuNvConfig, driver_version), 0);
    assert_eq!(offset_of!(VirtioGpuNvConfig, num_gpus), 32);
    assert_eq!(offset_of!(VirtioGpuNvConfig, caps), 36);
    assert_eq!(offset_of!(VirtioGpuNvConfig, gpu_device_ids), 40);
    assert_eq!(offset_of!(VirtioGpuNvConfig, gpus), 72);
    assert_eq!(offset_of!(GpuSlot, minor), 16);
    assert_eq!(offset_of!(GpuSlot, info_len), 20);
    assert_eq!(offset_of!(GpuSlot, info_text), 28);
}

/// The driver fails probe on num_gpus == 0, so a device that serves a default config never comes up.
#[test]
fn a_default_config_would_be_rejected_by_the_driver() {
    let cfg = VirtioGpuNvConfig::default();
    let n = cfg.num_gpus;
    assert_eq!(n, 0, "a config with no GPUs must not claim any");
}

#[test]
fn one_gpu_is_described_where_the_driver_looks() {
    let slot = GpuSlot::new("0000:01:00.0", 0, "Model: NVIDIA RTX A2000");
    let cfg = VirtioGpuNvConfig::new("615.71.09", &[slot], crate::caps::Caps::DEFAULT, 0);
    let bytes = cfg.as_bytes();

    assert_eq!(&bytes[0..9], b"615.71.09");
    assert_eq!(
        u32::from_le_bytes(bytes[36..40].try_into().unwrap()),
        crate::caps::Caps::DEFAULT.bits(),
        "caps at offset 36"
    );
    assert_eq!(u32::from_le_bytes(bytes[32..36].try_into().unwrap()), 1);
    assert_eq!(&bytes[72..84], b"0000:01:00.0");
    // info_len, at slot offset 20 within the slot array at 72.
    assert_eq!(
        u32::from_le_bytes(bytes[92..96].try_into().unwrap()),
        "Model: NVIDIA RTX A2000".len() as u32
    );
}

/// The driver NUL-terminates `pci_addr[15]` itself.
#[test]
fn an_over_long_pci_address_keeps_room_for_its_terminator() {
    let slot = GpuSlot::new("0000:01:00.0:extra", 0, "");
    assert_eq!(slot.pci_addr[PCI_ADDR_LEN - 1], 0);
    assert_eq!(&slot.pci_addr[..15], b"0000:01:00.0:ex");
}

#[test]
fn more_gpus_than_slots_are_truncated_not_over_claimed() {
    let many: Vec<_> = (0..12)
        .map(|i| GpuSlot::new(&format!("0000:0{i}:00.0"), i, ""))
        .collect();
    let cfg = VirtioGpuNvConfig::new("615.71.09", &many, crate::caps::Caps::DEFAULT, 0);
    let n = cfg.num_gpus;
    assert_eq!(
        n as usize, MAX_GPUS,
        "claimed more GPUs than it can describe"
    );
}

#[test]
fn a_read_past_the_end_is_clamped_rather_than_panicking() {
    let cfg = VirtioGpuNvConfig::default();
    assert_eq!(cfg.read(4012, 64).len(), 12);
    assert!(cfg.read(99_999, 16).is_empty());
    assert_eq!(cfg.read(0, 4024).len(), 4024);
}
