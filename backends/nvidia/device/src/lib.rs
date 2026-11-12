// crates/device/src/lib.rs VMM backend for virtio-gpu-nv.

pub mod caps;
pub mod error;
pub mod guarded;
pub mod guestmem;
pub mod handle_table;
pub mod host;
pub mod legion_window_pages;
pub mod mmap;
pub mod nvidia;
pub mod posture;
pub mod replay;
pub mod sandbox;
pub mod shm;
pub mod userspace;
pub mod virtio;
pub mod vram;
