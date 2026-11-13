//! Optional Wine dispatcher adapter.
use super::{PROCESS, SHARED, Shared, fail};
use core::sync::atomic::{AtomicU64, Ordering};
use legion_cpu_contract::{BYPASS_MAGIC, State, SyscallRoute, syscall};
const TABLE: usize = 0x7ffe_1000;
#[unsafe(no_mangle)]
static LEGION_WINE_ORIGINAL: AtomicU64 = AtomicU64::new(0);
core::arch::global_asm!(include_str!("wine_dispatch.S"));
unsafe extern "C" {
    fn legion_wine_entry();
}

#[repr(C)]
struct Frame {
    rax: u64,
    rbx: u64,
    rcx: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    xmm: [[u64; 2]; 16],
    flags: u64,
    return_address: u64,
    stack: u64,
    destination: u64,
    redirect: u64,
}
const _: () = assert!(
    core::mem::size_of::<Frame>() == 408
        && core::mem::offset_of!(Frame, xmm) == 112
        && core::mem::offset_of!(Frame, flags) == 368
        && core::mem::offset_of!(Frame, return_address) == 376
        && core::mem::offset_of!(Frame, redirect) == 400
);

/// # Safety Called only by the assembly entry with a complete exclusive initialized frame.
#[unsafe(no_mangle)]
unsafe extern "C" fn legion_wine_route(frame: *mut Frame) {
    // SAFETY: assembly owns a complete aligned frame through this call.
    let frame = unsafe { &mut *frame };
    // SAFETY: entry is published only after the retained shared state is ready.
    let shared = unsafe { &*SHARED.load(Ordering::Acquire) };
    let state = State {
        owner: shared.owner.load(Ordering::Acquire),
        declared_process_id: shared.payload.load(Ordering::Acquire),
        handler: shared.handler.load(Ordering::Acquire),
        ready: false,
    };
    let process = PROCESS.load(Ordering::Acquire);
    let return_address =
        if state.owner == process && state.handler != 0 && frame.xmm[5][0] != BYPASS_MAGIC {
            syscall_return(frame.return_address, frame.rax as u32)
        } else {
            frame.return_address
        };
    let result = syscall(
        state,
        process,
        frame.rax,
        return_address,
        frame.flags,
        frame.xmm[5],
    );
    frame.rax = result.rax;
    frame.rcx = result.rcx;
    frame.r11 = result.r11;
    frame.xmm[4] = result.xmm4;
    frame.xmm[5] = result.xmm5;
    frame.redirect = u64::from(result.route == SyscallRoute::Handler);
    if frame.redirect != 0 {
        shared.dispatch_redirected.fetch_add(1, Ordering::Relaxed);
    } else {
        shared.dispatch_forwarded.fetch_add(1, Ordering::Relaxed);
    }
}

struct ReturnEntry {
    logical: AtomicU64,
    physical: AtomicU64,
}
static RETURNS: [ReturnEntry; 1024] = [const {
    ReturnEntry {
        logical: AtomicU64::new(0),
        physical: AtomicU64::new(0),
    }
}; 1024];

fn syscall_return(logical: u64, service: u32) -> u64 {
    let begin = (logical as usize >> 4) & (RETURNS.len() - 1);
    for index in 0..16 {
        let entry = &RETURNS[(begin + index) & (RETURNS.len() - 1)];
        let key = entry.logical.load(Ordering::Acquire);
        if key == logical {
            return entry.physical.load(Ordering::Relaxed);
        }
        if key == 0 {
            break;
        }
    }
    let start = logical
        .checked_sub(31)
        .unwrap_or_else(|| fail(b"legion-wine-dispatch: invalid return address\n"));
    let mut bytes = [0_u8; 32];
    let local = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let remote = libc::iovec {
        iov_base: start as *mut core::ffi::c_void,
        iov_len: bytes.len(),
    };
    // SAFETY: bounded initialized output; the kernel validates the source.
    let read = unsafe { libc::process_vm_readv(libc::gettid(), &local, 1, &remote, 1, 0) };
    if read != 32 {
        // SAFETY: this normal native call owns its thread-local errno slot.
        let error = unsafe { *libc::__errno_location() };
        fail_stub(
            b"legion-wine-dispatch: unreadable NT stub (return/service/read/errno) ",
            &[logical, u64::from(service), read as u64, error as u64],
        );
    }
    let physical = decode_return(logical, service, &bytes)
        .unwrap_or_else(|| fail(b"legion-wine-dispatch: unsupported NT stub return layout\n"));
    for index in 0..16 {
        let entry = &RETURNS[(begin + index) & (RETURNS.len() - 1)];
        if entry
            .logical
            .compare_exchange(0, u64::MAX, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            entry.physical.store(physical, Ordering::Relaxed);
            entry.logical.store(logical, Ordering::Release);
            break;
        }
    }
    physical
}

fn fail_stub(message: &[u8], values: &[u64; 4]) -> ! {
    let mut output = [0_u8; 192];
    output[..message.len()].copy_from_slice(message);
    let mut used = message.len();
    for value in values {
        for shift in (0..16).rev() {
            output[used] = b"0123456789abcdef"[((value >> (shift * 4)) & 15) as usize];
            used += 1;
        }
        output[used] = b' ';
        used += 1;
    }
    output[used] = b'\n';
    fail(&output[..=used])
}

fn decode_return(logical: u64, service: u32, bytes: &[u8; 32]) -> Option<u64> {
    // Recognize the complete 32-byte Wine x64 stub, including selector test, both return branches, and the absolute CALL through the dispatcher slot.
    if bytes[..4] != [0x4c, 0x8b, 0xd1, 0xb8]
        || bytes[4..8] != service.to_le_bytes()
        || bytes[8..18] != [0xf6, 0x04, 0x25, 0x08, 0x03, 0xfe, 0x7f, 1, 0x75, 3]
        || bytes[18..24] != [0x0f, 5, 0xc3, 0xeb, 1, 0xc3]
        || bytes[24..] != [0xff, 0x14, 0x25, 0, 0x10, 0xfe, 0x7f, 0xc3]
    {
        return None;
    }
    logical.checked_sub(31)?.checked_add(20)
}

fn range(bytes: &[u8]) -> Option<(usize, usize)> {
    let at = bytes.iter().position(|b| *b == b'-')?;
    let parse = |s: &[u8]| usize::from_str_radix(core::str::from_utf8(s).ok()?, 16).ok();
    Some((parse(&bytes[..at])?, parse(&bytes[at + 1..])?))
}

fn mappings() -> Option<u64> {
    // SAFETY: fixed readonly proc file; the descriptor is closed below.
    let fd = unsafe {
        libc::open(
            c"/proc/self/maps".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return None;
    }
    let mut chunk = [0_u8; 4096];
    let mut line = [0_u8; 4096];
    let mut length = 0;
    let mut table = false;
    let mut original = 0_u64;
    let mut executable = false;
    let mut ranges = [(0_usize, 0_usize); 8];
    let mut range_count = 0;
    let mut total = 0;
    'read: loop {
        // SAFETY: owned fd and complete bounded writable output.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        total += n as usize;
        if total > 4 * 1024 * 1024 {
            break;
        }
        for byte in &chunk[..n as usize] {
            if *byte == b'\n' {
                let text = &line[..length];
                let mut words = text.split(|b| *b == b' ').filter(|v| !v.is_empty());
                let bounds = words.next().and_then(range);
                let permissions = words.next().unwrap_or_default();
                if let Some((start, end)) = bounds {
                    if start <= TABLE && end >= TABLE + 4096 && permissions == b"rw-p" {
                        table = true;
                        // SAFETY: verified complete private RW page; aligned pointer slot is initialized by Wine before KUSER registration.
                        original =
                            unsafe { (&*(TABLE as *const AtomicU64)).load(Ordering::Acquire) };
                    }
                    if original != 0
                        && start <= original as usize
                        && (original as usize) < end
                        && permissions == b"r-xp"
                        && text.ends_with(b"/ntdll.so")
                    {
                        executable = true;
                    }
                    if permissions == b"r-xp"
                        && text.ends_with(b"/ntdll.so")
                        && range_count < ranges.len()
                    {
                        ranges[range_count] = (start, end);
                        range_count += 1;
                    }
                }
                length = 0;
            } else if length < line.len() {
                line[length] = *byte;
                length += 1;
            } else {
                break 'read;
            }
        }
    }
    // SAFETY: this worker owns the proc descriptor.
    unsafe {
        libc::close(fd);
    }
    executable |= ranges[..range_count]
        .iter()
        .any(|(start, end)| *start <= original as usize && (original as usize) < *end);
    (table && executable).then_some(original)
}

pub(super) fn install(shared: &Shared) {
    // SAFETY: fixed environment name; non-null getenv result is a C string.
    let enabled = unsafe { libc::getenv(c"LEGION_WINE_DISPATCH".as_ptr()) };
    // SAFETY: non-null getenv result is a complete process-owned C string.
    if enabled.is_null() || unsafe { libc::strcmp(enabled, c"1".as_ptr()) } != 0 {
        return;
    }
    let original = mappings()
        .unwrap_or_else(|| fail(b"legion-cpu-adapter: unsupported Wine dispatcher mapping\n"));
    LEGION_WINE_ORIGINAL.store(original, Ordering::Release);
    // SAFETY: mappings verified this aligned RW private slot and its original executable ntdll entry.
    let slot = unsafe { &*(TABLE as *const AtomicU64) };
    if slot
        .compare_exchange(
            original,
            legion_wine_entry as *const () as u64,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        fail(b"legion-cpu-adapter: Wine dispatcher changed during installation\n");
    }
    shared.dispatch_installed.store(1, Ordering::Release);
}

#[cfg(test)]
#[path = "../tests/unit/wine_dispatch.rs"]
mod tests;
