//! The one place a guest's ioctl reaches the host driver.
use std::os::fd::RawFd;

/// The host driver, as dispatch sees it.
pub trait HostDriver: Send {
    /// Issue `request` on `fd` with `arg` as the parameter block.
    fn ioctl(&self, fd: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32>;
}

/// The real thing: `ioctl(2)` on the host's own descriptor.
pub struct RealHost;

impl HostDriver for RealHost {
    fn ioctl(&self, fd: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
        // SAFETY: `arg` is a live, writable buffer for the whole call, sized by the caller to what `request` declares or longer.
        let rc = unsafe { libc::ioctl(fd, request as libc::Ioctl, arg.as_mut_ptr()) };
        if rc < 0 {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
        } else {
            Ok(())
        }
    }
}
