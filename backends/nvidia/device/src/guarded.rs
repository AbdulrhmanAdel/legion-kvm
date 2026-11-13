//! A buffer the host driver writes into, with a guard page behind it.

use std::ptr;

pub struct GuardedBuf {
    base: *mut u8,
    mapped: usize,
    offset: usize,
    len: usize,
}

// SAFETY: a GuardedBuf owns its mapping outright; nothing else holds the pointer, so moving it to another thread moves sole ownership with it.
unsafe impl Send for GuardedBuf {}

const PAGE: usize = 4096;

/// Readable bytes after the buffer, standing in for the caller's own memory.
const SLACK: usize = 4096;

impl GuardedBuf {
    /// A buffer of `len` bytes, followed by a page of readable slack and then a `PROT_NONE` page.
    pub fn new(len: usize) -> Option<Self> {
        if len == 0 {
            return None;
        }
        let pages = (len + SLACK).div_ceil(PAGE);
        let mapped = (pages + 1) * PAGE;
        let base = unsafe {
            libc::mmap(
                ptr::null_mut(),
                mapped,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return None;
        }
        let base = base as *mut u8;
        // The last page is the guard.
        let guard = unsafe { base.add(pages * PAGE) };
        if unsafe { libc::mprotect(guard as *mut libc::c_void, PAGE, libc::PROT_NONE) } != 0 {
            unsafe { libc::munmap(base as *mut libc::c_void, mapped) };
            return None;
        }
        Some(Self {
            base,
            mapped,
            offset: 0,
            len,
        })
    }

    /// Bytes this mapping can hold with the full slack still behind them.
    fn capacity(&self) -> usize {
        self.mapped - PAGE - SLACK
    }

    /// Reuse the mapping for `len` bytes, placed so that the slack and the guard sit right behind them, as they would in a fresh buffer of that size.
    fn reset(&mut self, len: usize) -> bool {
        if len == 0 || len > self.capacity() {
            return false;
        }
        // 16-byte aligned, as a fresh mapping's start is (and more than any parameter block needs).
        self.offset = (self.capacity() - len) & !15;
        self.len = len;
        let tail = self.mapped - PAGE - self.offset - len;
        unsafe { ptr::write_bytes(self.base.add(self.offset + len), 0, tail) };
        true
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        unsafe { self.base.add(self.offset) }
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.base.add(self.offset), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.base.add(self.offset), self.len) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A few guarded buffers kept for reuse.
#[derive(Default)]
pub struct GuardPool {
    free: Vec<GuardedBuf>,
}

impl GuardPool {
    /// Buffers kept, and the largest kept.
    const KEEP: usize = 8;
    const KEEP_MAX: usize = 64 * 1024;

    /// A buffer for `len` bytes: a kept one if any is big enough, else fresh.
    pub fn take(&mut self, len: usize) -> Option<GuardedBuf> {
        let best = self
            .free
            .iter()
            .enumerate()
            .filter(|(_, b)| b.capacity() >= len)
            .min_by_key(|(_, b)| b.capacity())
            .map(|(i, _)| i);
        if let Some(i) = best {
            let mut b = self.free.swap_remove(i);
            if b.reset(len) {
                return Some(b);
            }
        }
        GuardedBuf::new(len)
    }

    /// Hand a buffer back.
    pub fn give(&mut self, b: GuardedBuf) {
        if self.free.len() < Self::KEEP && b.capacity() <= Self::KEEP_MAX {
            self.free.push(b);
        }
    }
}

/// A pooled buffer that goes back to its pool when dropped, so every early return from a handler gives it back too.
pub struct Lease<'a> {
    buf: Option<GuardedBuf>,
    pool: &'a std::cell::RefCell<GuardPool>,
}

impl GuardPool {
    pub fn lease(pool: &std::cell::RefCell<GuardPool>, len: usize) -> Option<Lease<'_>> {
        let buf = pool.borrow_mut().take(len)?;
        Some(Lease {
            buf: Some(buf),
            pool,
        })
    }
}

impl std::ops::Deref for Lease<'_> {
    type Target = GuardedBuf;
    fn deref(&self) -> &GuardedBuf {
        self.buf.as_ref().expect("held until drop")
    }
}

impl std::ops::DerefMut for Lease<'_> {
    fn deref_mut(&mut self) -> &mut GuardedBuf {
        self.buf.as_mut().expect("held until drop")
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Some(b) = self.buf.take() {
            self.pool.borrow_mut().give(b);
        }
    }
}

impl Drop for GuardedBuf {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.mapped) };
    }
}

#[cfg(test)]
#[path = "../tests/unit/guarded.rs"]
mod tests;
