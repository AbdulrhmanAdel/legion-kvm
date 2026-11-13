//! Guest RAM, as the backend can reach it, and how a guest's pages become one host address.

use std::os::fd::{AsRawFd, OwnedFd};

/// The most runs one registration may carry.
pub const MAX_RUNS: usize = 1024;

/// The most one registration may cover.
pub const MAX_BYTES: u64 = 256 * 1024 * 1024;

const PAGE: u64 = 4096;

/// Where one guest-physical address lives in the backend.
#[derive(Debug)]
pub struct Backing {
    /// The guest RAM region's file, this caller's own handle on it.
    pub fd: OwnedFd,
    /// Byte offset of this address within that file.
    pub offset: u64,
    /// How many bytes from here stay inside the same region.
    pub len: u64,
}

/// Guest RAM, as something the backend can map from.
pub trait GuestRam: Send {
    /// Where `gpa` lives, or `None` if it is not guest RAM at all.
    fn backing(&self, gpa: u64) -> Option<Backing>;
}

/// One run of guest-physical memory: an address and a length, both in pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub gpa: u64,
    pub len: u64,
}

/// A contiguous host mapping that aliases a guest's pages.
#[derive(Debug)]
pub struct Stitched {
    addr: u64,
    len: usize,
}

impl Stitched {
    /// The address to give RM.
    pub fn addr(&self) -> u64 {
        self.addr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Stitched {
    fn drop(&mut self) {
        // SAFETY: this span was reserved by `stitch` and nothing else has been mapped into it; every run was placed inside it with MAP_FIXED.
        let rc = unsafe { libc::munmap(self.addr as *mut libc::c_void, self.len) };
        if rc != 0 {
            log::error!(
                "unmapping a registered span at {:#x}+{:#x} failed: {}",
                self.addr,
                self.len,
                std::io::Error::last_os_error()
            );
        }
    }
}

// SAFETY: `Stitched` owns a mapping and hands out only its address and length.
unsafe impl Send for Stitched {}

/// Build one host address that aliases exactly these guest pages.
pub fn stitch(ram: &dyn GuestRam, runs: &[Run], want: u64) -> Result<Stitched, String> {
    if runs.is_empty() {
        return Err("no pages were sent for it".into());
    }
    if runs.len() > MAX_RUNS {
        return Err(format!(
            "it is described by {} runs and this backend carries {MAX_RUNS}",
            runs.len()
        ));
    }
    if want == 0 || want > MAX_BYTES {
        return Err(format!("it is {want} bytes, and the limit is {MAX_BYTES}"));
    }
    if !want.is_multiple_of(PAGE) {
        return Err(format!("it is {want} bytes, which is not whole pages"));
    }

    // Checked before anything is mapped, so a refusal leaves nothing behind.
    let mut total: u64 = 0;
    for (i, r) in runs.iter().enumerate() {
        if r.len == 0 {
            return Err(format!("run {i} is empty"));
        }
        if !r.gpa.is_multiple_of(PAGE) || !r.len.is_multiple_of(PAGE) {
            return Err(format!(
                "run {i} is {:#x}+{:#x}, which is not whole pages",
                r.gpa, r.len
            ));
        }
        if r.gpa.checked_add(r.len).is_none() {
            return Err(format!(
                "run {i} at {:#x} is {:#x} long and wraps",
                r.gpa, r.len
            ));
        }
        total = total
            .checked_add(r.len)
            .ok_or_else(|| "the runs add up to more than an address space".to_string())?;
    }
    if total != want {
        // Short would have RM pin past what the guest sent; long would have it pin pages the registration never covered.
        return Err(format!(
            "the pages sent cover {total} bytes and it is {want}"
        ));
    }

    // One reservation, so the runs land next to each other whatever the kernel would otherwise have chosen, and so a failure half way through unmaps as one span.
    let len = want as usize;
    // SAFETY: an anonymous reservation at an address of the kernel's choosing.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(format!(
            "reserving {len} bytes for it failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let span = Stitched {
        addr: base as u64,
        len,
    };

    let mut at: u64 = 0;
    for (i, r) in runs.iter().enumerate() {
        let Some(b) = ram.backing(r.gpa) else {
            return Err(format!(
                "run {i} at {:#x} is not this guest's memory",
                r.gpa
            ));
        };
        // The run has to fit in the region it starts in.
        if r.len > b.len {
            return Err(format!(
                "run {i} at {:#x} is {:#x} long and only {:#x} of it is in one region",
                r.gpa, r.len, b.len
            ));
        }
        // SAFETY: inside the reservation above -- `total == want == len` and `at` has advanced by exactly the runs already placed.
        let got = unsafe {
            libc::mmap(
                (span.addr + at) as *mut libc::c_void,
                r.len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED,
                b.fd.as_raw_fd(),
                b.offset as libc::off_t,
            )
        };
        if got == libc::MAP_FAILED {
            return Err(format!(
                "mapping run {i} at {:#x} failed: {}",
                r.gpa,
                std::io::Error::last_os_error()
            ));
        }
        at += r.len;
    }

    Ok(span)
}

/// Guest RAM the backend's own tests can run against, so a registration can be followed all the way to a host address without a VM.
#[cfg(test)]
#[path = "../tests/support/guest_ram.rs"]
pub(crate) mod fake;

#[cfg(test)]
#[path = "../tests/unit/guestmem.rs"]
mod tests;
