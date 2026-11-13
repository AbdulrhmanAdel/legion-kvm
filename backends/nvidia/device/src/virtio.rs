//! What the guest driver expects to find on the bus.
pub const VIRTIO_ID_GPU_NV: u32 = 45;

/// Virtqueue count.
pub const NUM_QUEUES: usize = 2;

/// Guest requests, device replies.
pub const CONTROL_QUEUE: usize = 0;
/// Device-initiated notifications to the guest.
pub const EVENT_QUEUE: usize = 1;

/// Recommended virtqueue size.
pub const QUEUE_SIZE: u16 = 256;

/// Longest PCI address the driver will store, including its NUL.
pub const PCI_ADDR_LEN: usize = 16;
/// Bytes of `/proc/driver/nvidia/gpus/<addr>/information` carried per GPU. 448, not the 1060 this began as.
pub const INFO_TEXT_LEN: usize = 448;
/// Driver version string length in config space, including its NUL.
pub const DRIVER_VERSION_LEN: usize = 32;
/// GPU slots in config space.
pub const MAX_GPUS: usize = 8;
/// FD translation entries in config space.
pub const MAX_FD_TRANSLATIONS: usize = 16;

/// One GPU, as the guest driver reads it.
#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub struct GpuSlot {
    /// Directory name under `/proc/driver/nvidia/gpus`, NUL-terminated.
    pub pci_addr: [u8; PCI_ADDR_LEN],
    /// The `N` in `/dev/nvidiaN`.
    pub minor: u32,
    /// Valid bytes in `info_text`.
    pub info_len: u32,
    pub padding: [u32; 1],
    /// Raw contents of that GPU's `information` file.
    pub info_text: [u8; INFO_TEXT_LEN],
}

impl Default for GpuSlot {
    fn default() -> Self {
        Self {
            pci_addr: [0; PCI_ADDR_LEN],
            minor: 0,
            info_len: 0,
            padding: [0; 1],
            info_text: [0; INFO_TEXT_LEN],
        }
    }
}

impl GpuSlot {
    /// Build a slot, truncating both strings to what the driver can hold.
    pub fn new(pci_addr: &str, minor: u32, info_text: &str) -> Self {
        let mut slot = Self {
            minor,
            ..Default::default()
        };
        // Leave room for the NUL the driver writes at the last byte.
        let addr = pci_addr.as_bytes();
        let n = addr.len().min(PCI_ADDR_LEN - 1);
        slot.pci_addr[..n].copy_from_slice(&addr[..n]);

        let info = info_text.as_bytes();
        let n = info.len().min(INFO_TEXT_LEN);
        slot.info_text[..n].copy_from_slice(&info[..n]);
        slot.info_len = n as u32;
        slot
    }
}

use abi::ioctl::{
    NV_ESC_ALLOC_OS_EVENT, NV_ESC_FREE_OS_EVENT, NV_ESC_REGISTER_FD, NV_ESC_RM_ALLOC_MEMORY,
    NV_ESC_RM_MAP_MEMORY,
};

/// The ioctls that carry a file descriptor, and the byte offset it sits at.
pub const FD_CARRYING_IOCTLS: &[(u32, u32)] = &[
    (NV_ESC_REGISTER_FD, 0),
    (NV_ESC_ALLOC_OS_EVENT, 8),
    (NV_ESC_FREE_OS_EVENT, 8),
    (NV_ESC_RM_ALLOC_MEMORY, 48),
    (NV_ESC_RM_MAP_MEMORY, 48),
];

/// One ioctl the device wants the driver to rewrite file descriptors in.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C, packed)]
pub struct FdTranslation {
    /// The ioctl number this applies to.
    pub nr: u32,
    /// Where in the payload the descriptor sits.
    pub payload_offset: u32,
}

/// This backend reads a deep block that holds several segments, one per pointer inside an RM control's parameters, announced by `deep_ptr_offset == protocol::segments::SEGMENTED`.
pub const FEATURE_RMCTRL_SEGMENTS: u32 = 1 << 0;

/// Device configuration space.
#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub struct VirtioGpuNvConfig {
    /// Host driver version, NUL-terminated, e.g.
    pub driver_version: [u8; DRIVER_VERSION_LEN],
    /// How many entries of `gpus` are valid.
    pub num_gpus: u32,
    /// What the guest is served, as `crate::caps` bits.
    pub caps: u32,
    /// PCI device id per GPU.
    pub gpu_device_ids: [u32; MAX_GPUS],
    pub gpus: [GpuSlot; MAX_GPUS],
    pub num_fd_translations: u32,
    /// What this backend can do beyond v0.1, as [`FEATURE_RMCTRL_SEGMENTS`] and the like.
    pub features: u32,
    pub fd_translations: [FdTranslation; MAX_FD_TRANSLATIONS],
    /// The video memory limit this backend enforces, in MiB; 0 for none.
    pub vram_limit_mib: u64,
}

impl Default for VirtioGpuNvConfig {
    fn default() -> Self {
        Self {
            driver_version: [0; DRIVER_VERSION_LEN],
            num_gpus: 0,
            caps: 0,
            gpu_device_ids: [0; MAX_GPUS],
            gpus: [GpuSlot::default(); MAX_GPUS],
            num_fd_translations: 0,
            features: 0,
            fd_translations: [FdTranslation::default(); MAX_FD_TRANSLATIONS],
            vram_limit_mib: 0,
        }
    }
}

impl VirtioGpuNvConfig {
    /// Build config space for a set of host GPUs.
    pub fn new(
        driver_version: &str,
        gpus: &[GpuSlot],
        caps: crate::caps::Caps,
        vram_limit_mib: u64,
    ) -> Self {
        let mut cfg = Self {
            caps: caps.bits(),
            vram_limit_mib,
            features: FEATURE_RMCTRL_SEGMENTS,
            ..Self::default()
        };
        let v = driver_version.as_bytes();
        let n = v.len().min(DRIVER_VERSION_LEN - 1);
        cfg.driver_version[..n].copy_from_slice(&v[..n]);

        let n = gpus.len().min(MAX_GPUS);
        cfg.gpus[..n].copy_from_slice(&gpus[..n]);
        cfg.num_gpus = n as u32;

        // Tell the driver which ioctls carry a file descriptor, and where.
        for (i, (nr, off)) in FD_CARRYING_IOCTLS.iter().enumerate() {
            cfg.fd_translations[i] = FdTranslation {
                nr: *nr,
                payload_offset: *off,
            };
        }
        cfg.num_fd_translations = FD_CARRYING_IOCTLS.len() as u32;
        cfg
    }

    /// Config space as the bytes a guest reads.
    pub fn as_bytes(&self) -> &[u8] {
        // Safe: `repr(C, packed)` with no padding and no pointers, so every byte of the struct is initialised and meaningful.
        unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        }
    }

    /// Serve a config read, clamped to the struct.
    pub fn read(&self, offset: u32, size: u32) -> Vec<u8> {
        let bytes = self.as_bytes();
        let start = (offset as usize).min(bytes.len());
        let end = start.saturating_add(size as usize).min(bytes.len());
        bytes[start..end].to_vec()
    }
}

#[cfg(test)]
#[path = "../tests/unit/virtio.rs"]
mod tests;
