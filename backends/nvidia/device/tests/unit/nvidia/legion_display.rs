use super::*;
struct RefusingKernel;
impl HostDriver for RefusingKernel {
    fn ioctl(&self, fd: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
        assert_eq!(fd, 17);
        assert_eq!(request, 0xc00c_642d);
        assert_eq!(arg.len(), 12);
        assert_eq!(word(arg, 0), 7);
        let flags = word(arg, 4);
        assert_ne!(flags & u32::try_from(libc::O_CLOEXEC).expect("flag"), 0);
        assert_eq!(
            flags & u32::try_from(libc::O_ACCMODE).expect("mode"),
            u32::try_from(libc::O_RDWR).expect("RDWR")
        );
        assert_eq!(i32::from_le_bytes(arg[8..].try_into().expect("FD")), -1);
        Err(libc::EACCES)
    }
}
#[test]
fn prime_export_uses_real_drm_flags_and_preserves_kernel_error() {
    assert_eq!(
        export_owned_dmabuf(&RefusingKernel, 17, 7).err(),
        Some(libc::EACCES)
    );
}
#[test]
#[cfg(not(miri))] // The backend constructor creates a real SHM memfd.
fn display_requests_on_non_drm_handles_fail_without_calling_the_kernel() {
    let mut backend = NvidiaBackend::with_default_zones();
    backend.current_handle = 17;
    backend.handle_kinds.insert(17, DeviceKind::Ctl);
    backend.set_host(Box::new(RefusingKernel));
    let mut response = [0_u8; 64];
    let n = backend.handle_legion_display(-1, 0xc030_4c02, &[0; 48], &mut response);
    assert_eq!(n, 16);
    assert!(i32::from_le_bytes(response[8..12].try_into().expect("status")) < 0);
}
