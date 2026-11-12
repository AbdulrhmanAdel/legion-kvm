// crates/protocol/src/messages.rs Wire message types for virtio-gpu-nv.

// --------------------------------------------------------------------------- Message type discriminants ---------------------------------------------------------------------------

/// Identifies the kind of message in a `MsgHeader`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgType {
    /// Guest → host: open a `/dev/nvidia*` device.
    Open = 1,

    Close = 2,
    /// Guest → host: forward a raw ioctl.
    Ioctl = 3,
    /// Guest → host: map device memory into the shared window.
    Mmap = 4,
    /// Guest → host: release a mapping made by `Mmap`.
    Munmap = 5,
    /// Guest → host: the contents of the host's `/proc/driver/nvidia` tree, which the guest republishes so its own userspace can read them.
    GetProcFiles = 6,
    /// Guest → host: the same for the sysfs attributes the userspace driver looks for.
    GetSysFiles = 7,
    /// **Host → guest**, on the event queue: the descriptor named by `MsgHeader::handle` has something to report.
    EventReady = 8,
}

impl MsgType {
    /// Decode a wire discriminant.
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            1 => Self::Open,
            2 => Self::Close,
            3 => Self::Ioctl,
            4 => Self::Mmap,
            5 => Self::Munmap,
            6 => Self::GetProcFiles,
            7 => Self::GetSysFiles,
            8 => Self::EventReady,
            _ => return None,
        })
    }
}

// --------------------------------------------------------------------------- Common header, used for both directions ---------------------------------------------------------------------------

/// Every message starts with this header, in both directions.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MsgHeader {
    /// One of the `MsgType` values.
    pub msg_type: u32,
    /// On a request, the handle to act on.
    pub handle: u32,
    /// Result, and **signed**: zero on success, negative errno on failure.
    pub status: i32,
    pub padding: u32,
}

impl MsgHeader {
    /// A success response carrying `handle`.
    pub fn ok(msg_type: MsgType, handle: u32) -> Self {
        Self {
            msg_type: msg_type as u32,
            handle,
            status: 0,
            padding: 0,
        }
    }

    /// A failure response.
    pub fn err(msg_type: MsgType, errno: i32) -> Self {
        Self {
            msg_type: msg_type as u32,
            handle: 0,
            status: -errno.abs(),
            padding: 0,
        }
    }
}

// --------------------------------------------------------------------------- Device identification ---------------------------------------------------------------------------

/// `/dev/nvidiactl`.
pub const DEV_CTL: u32 = 255;
/// `/dev/nvidia-uvm`.
pub const DEV_UVM: u32 = 256;
/// `/dev/nvidia-uvm-tools`.
pub const DEV_UVM_TOOLS: u32 = 257;
/// `/dev/nvidia-modeset`.
pub const DEV_MODESET: u32 = 258;
/// Render nodes start here.
pub const DEV_DRI_BASE: u32 = 512;
/// Highest GPU index expressible before the control device's value.
pub const MAX_GPU_INDEX: u32 = 254;

/// Which device an `Open` refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    /// `/dev/nvidiaN`, where N is the guest's minor number.
    Gpu(u32),
    Ctl,
    Uvm,
    UvmTools,
    Modeset,
    /// A DRM render node, by its index in the list the device reported.
    Dri(u32),
}

impl DeviceKind {
    /// Decode the `device_type` field of an `OpenReq`.
    pub fn from_device_type(v: u32) -> Option<Self> {
        Some(match v {
            0..=MAX_GPU_INDEX => Self::Gpu(v),
            DEV_CTL => Self::Ctl,
            DEV_UVM => Self::Uvm,
            DEV_UVM_TOOLS => Self::UvmTools,
            DEV_MODESET => Self::Modeset,
            _ if v >= DEV_DRI_BASE => Self::Dri(v - DEV_DRI_BASE),
            _ => return None,
        })
    }
}

// --------------------------------------------------------------------------- OPEN ---------------------------------------------------------------------------

/// Request payload for `MsgType::Open`, following a `MsgHeader`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenReq {
    /// See [`DeviceKind::from_device_type`].
    pub device_type: u32,
    /// The guest's `open(2)` flags.
    pub flags: u32,
}

// --------------------------------------------------------------------------- IOCTL ---------------------------------------------------------------------------

/// Request payload for `MsgType::Ioctl`, following a `MsgHeader`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IoctlReq {
    /// The ioctl request number.
    pub cmd: u32,
    /// Bytes of top-level parameter struct following this header.
    pub data_len: u32,
    /// Where the nested block begins, measured from the start of the payload -- so in practice always equal to `data_len`, since the driver lays the nested block immediately after the top-level struct (`req->nested_offset = cpu_to_le32(sizeof(params))`).
    pub nested_offset: u32,
    /// Bytes of nested data following the top-level struct.
    pub nested_len: u32,
    /// Where, inside the nested block, a further pointer sits, when the nested block carries one.
    pub deep_ptr_offset: u32,
    /// Bytes of a second-level block following the nested block: what the pointer at `deep_ptr_offset` points at in the guest.
    pub deep_len: u32,
}

/// Response payload for `MsgType::Ioctl`, following a `MsgHeader`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IoctlResp {
    pub data_len: u32,
    pub nested_len: u32,
    /// Bytes of second-level block following the nested block, for the guest to copy back to where its own pointer points.
    pub deep_len: u32,
}

// --------------------------------------------------------------------------- MMAP / MUNMAP ---------------------------------------------------------------------------

/// Request payload for `MsgType::Mmap`, following a `MsgHeader`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MmapReq {
    pub size: u64,
    pub offset: u64,
    pub prot: u32,
    pub padding: u32,
}

/// Response payload for `MsgType::Mmap`, following a `MsgHeader`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MmapResp {
    /// Where the guest should map from, in its own physical address space.
    pub guest_phys_addr: u64,
    pub size: u64,
    /// Identifier the guest passes back to `Munmap`.
    pub mapping_id: u32,
    pub padding: u32,
}

/// Request payload for `MsgType::Munmap`, following a `MsgHeader`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MunmapReq {
    pub mapping_id: u32,
    pub padding: u32,
}

// --------------------------------------------------------------------------- GET_PROC_FILES / GET_SYS_FILES ---------------------------------------------------------------------------

/// One file in the response to `GetProcFiles` or `GetSysFiles`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FileEntry {
    /// Zero marks the end of the stream.
    pub path_len: u32,
    pub content_len: u32,
}

// --------------------------------------------------------------------------- Size assertions (compile-time, no_std compatible) --------------------------------------------------------------------------- These catch accidental padding changes that would break the C header.

const _: () = {
    assert!(size_of::<MsgHeader>() == 16);
    assert!(size_of::<OpenReq>() == 8);
    assert!(size_of::<IoctlReq>() == 24);
    assert!(size_of::<IoctlResp>() == 12);
    assert!(size_of::<MmapReq>() == 24);
    assert!(size_of::<MmapResp>() == 24);
    assert!(size_of::<MunmapReq>() == 8);
    assert!(size_of::<FileEntry>() == 8);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_types_decode_the_way_the_driver_encodes_them() {
        assert_eq!(DeviceKind::from_device_type(0), Some(DeviceKind::Gpu(0)));
        assert_eq!(DeviceKind::from_device_type(3), Some(DeviceKind::Gpu(3)));
        assert_eq!(DeviceKind::from_device_type(255), Some(DeviceKind::Ctl));
        assert_eq!(DeviceKind::from_device_type(256), Some(DeviceKind::Uvm));
        assert_eq!(
            DeviceKind::from_device_type(257),
            Some(DeviceKind::UvmTools)
        );
        assert_eq!(DeviceKind::from_device_type(258), Some(DeviceKind::Modeset));
    }

    /// A render node is how NVIDIA's Vulkan userspace finds the GPU, so it is addressable; the gap between the singletons and the render nodes is not.
    #[test]
    fn render_nodes_are_addressable_and_nonsense_is_not() {
        assert_eq!(
            DeviceKind::from_device_type(DEV_DRI_BASE),
            Some(DeviceKind::Dri(0))
        );
        assert_eq!(
            DeviceKind::from_device_type(DEV_DRI_BASE + 3),
            Some(DeviceKind::Dri(3))
        );
        assert_eq!(DeviceKind::from_device_type(300), None);
    }

    /// The driver tests `(s32)status < 0`.
    #[test]
    fn an_error_status_is_negative() {
        let h = MsgHeader::err(MsgType::Open, 2);
        assert_eq!(h.status, -2);
        assert!(h.status < 0);
        assert_eq!(MsgHeader::err(MsgType::Open, -2).status, -2);
        assert_eq!(MsgHeader::ok(MsgType::Open, 7).status, 0);
        assert_eq!(MsgHeader::ok(MsgType::Open, 7).handle, 7);
    }

    #[test]
    fn message_types_round_trip() {
        for t in [
            MsgType::Open,
            MsgType::Close,
            MsgType::Ioctl,
            MsgType::Mmap,
            MsgType::Munmap,
            MsgType::GetProcFiles,
            MsgType::GetSysFiles,
            MsgType::EventReady,
        ] {
            assert_eq!(MsgType::from_u32(t as u32), Some(t));
        }
        assert_eq!(MsgType::from_u32(0), None);
        assert_eq!(MsgType::from_u32(9), None);
    }
}
