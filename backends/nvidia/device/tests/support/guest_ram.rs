use super::*;
use std::os::fd::FromRawFd;

/// Guest RAM made of memfds, one per region, so the stitcher can be run against something whose contents are known.
pub(crate) struct FakeRam {
    regions: Vec<(u64, u64, OwnedFd)>, // base, len, fd
}

impl FakeRam {
    pub(crate) fn new(spans: &[(u64, u64)]) -> Self {
        let regions = spans
            .iter()
            .map(|&(base, len)| {
                let raw = unsafe { libc::memfd_create(c"guest-ram".as_ptr(), libc::MFD_CLOEXEC) };
                assert!(raw >= 0);
                assert_eq!(unsafe { libc::ftruncate(raw, len as libc::off_t) }, 0);
                (base, len, unsafe { OwnedFd::from_raw_fd(raw) })
            })
            .collect();
        Self { regions }
    }

    /// Fill every page with a byte derived from its guest address, so a stitched span can be checked page by page.
    pub(crate) fn fill(&self) {
        for &(base, len, ref fd) in &self.regions {
            for page in 0..len / PAGE {
                let b = [mark(base + page * PAGE); PAGE as usize];
                let n = unsafe {
                    libc::pwrite(
                        fd.as_raw_fd(),
                        b.as_ptr() as *const libc::c_void,
                        PAGE as usize,
                        (page * PAGE) as libc::off_t,
                    )
                };
                assert_eq!(n, PAGE as isize);
            }
        }
    }
}

pub(crate) fn mark(gpa: u64) -> u8 {
    (gpa / PAGE) as u8 ^ 0xa5
}

impl GuestRam for FakeRam {
    fn backing(&self, gpa: u64) -> Option<Backing> {
        self.regions
            .iter()
            .find(|&&(base, len, _)| gpa >= base && gpa < base + len)
            .map(|&(base, len, ref fd)| Backing {
                fd: fd.try_clone().expect("duplicating a memfd"),
                offset: gpa - base,
                len: base + len - gpa,
            })
    }
}
