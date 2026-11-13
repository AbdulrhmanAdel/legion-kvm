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
