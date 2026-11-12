//! Opt-in resolver re-entry compatibility bridge.
use super::{Shared, fail};
use core::{
    cell::UnsafeCell,
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static BUSY: AtomicBool = AtomicBool::new(false);
static SITE: AtomicUsize = AtomicUsize::new(0);
static DIAGNOSTIC_STAGE: AtomicUsize = AtomicUsize::new(0);
#[unsafe(no_mangle)]
static LEGION_RESOLVER_GATEWAY: AtomicUsize = AtomicUsize::new(0);
#[unsafe(no_mangle)]
static LEGION_RESOLVER_HOOK_POINTER: AtomicUsize = AtomicUsize::new(0);
#[unsafe(no_mangle)]
static LEGION_RESOLVER_STATE: AtomicPtr<Shared> = AtomicPtr::new(ptr::null_mut());
core::arch::global_asm!(include_str!("resolver.S"));
unsafe extern "C" {
    fn legion_resolver_entry();
    fn legion_resolver_entry_end();
    fn legion_resolver_clear_depth();
}

struct Observer {
    kernel: [usize; 4],
    ntdll: [usize; 4],
    export: usize,
    pristine: [u8; 5],
    function: [u32; 3],
    // Version 1/UHANDLER, 17-byte prologue, four unwind-code slots, RBP=RSP+160.
    unwind: [u32; 4],
}
struct Storage(UnsafeCell<Observer>);
// SAFETY: BUSY grants exclusive access; published function/unwind fields become immutable before RtlAddFunctionTable registers their process-lifetime storage.
unsafe impl Sync for Storage {}
static OBSERVER: Storage = Storage(UnsafeCell::new(Observer {
    kernel: [0; 4],
    ntdll: [0; 4],
    export: 0,
    pristine: [0; 5],
    function: [0; 3],
    unwind: [
        u32::from_le_bytes([0x11, 17, 4, 0xa5]),
        u32::from_le_bytes([17, 3, 9, 1]),
        u32::from_le_bytes([20, 0, 1, 0x50]),
        0,
    ],
}));
const _: () = assert!(
    core::mem::offset_of!(Shared, resolver_installed) == 152
        && core::mem::offset_of!(Shared, resolver_outer) == 160
        && core::mem::offset_of!(Shared, resolver_nested) == 168
);

pub(super) fn requested() -> bool {
    // SAFETY: fixed environment name, non-null result is NUL terminated.
    let value = unsafe { libc::getenv(c"LEGION_RESOLVER_BRIDGE".as_ptr()) };
    // SAFETY: getenv returned a complete process-owned C string.
    !value.is_null() && unsafe { libc::strcmp(value, c"1".as_ptr()) } == 0
}
pub(super) fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}
pub(super) fn enable(shared: &Shared) {
    LEGION_RESOLVER_STATE.store(ptr::from_ref(shared).cast_mut(), Ordering::Release);
    ENABLED.store(true, Ordering::Release);
}

pub(super) unsafe fn handle(info: *mut libc::siginfo_t, context: *mut c_void) -> bool {
    let site = SITE.load(Ordering::Acquire);
    if site == 0 || info.is_null() || context.is_null() {
        return false;
    }
    // SAFETY: the caller owns live signal frames; edit only the resumed RIP.
    let (code, frame) = unsafe { ((*info).si_code, &mut *context.cast::<libc::ucontext_t>()) };
    // Linux x86 UAPI ILL_ILLOPN (UD2); libc does not export this constant.
    if code != 2 || frame.uc_mcontext.gregs[libc::REG_RIP as usize] as usize != site {
        return false;
    }
    frame.uc_mcontext.gregs[libc::REG_RIP as usize] =
        legion_resolver_entry as *const () as libc::greg_t;
    true
}

fn read<const N: usize>(at: usize) -> Option<[u8; N]> {
    if at == 0 || at.checked_add(N)? >= 1_usize << 47 {
        return None;
    }
    let mut bytes = [0; N];
    let local = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: N,
    };
    let remote = libc::iovec {
        iov_base: at as *mut c_void,
        iov_len: N,
    };
    // SAFETY: a bounded owned output; process_vm_readv checks source readability without dereferencing an untrusted PE pointer in this process.
    (unsafe { libc::process_vm_readv(libc::gettid(), &local, 1, &remote, 1, 0) } == N as isize)
        .then_some(bytes)
}
fn word(bytes: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?) as usize)
}
fn export(base: usize, dll: &[u8], name: &[u8]) -> Option<usize> {
    let dos = read::<64>(base)?;
    if &dos[..2] != b"MZ" {
        return None;
    }
    let pe = word(&dos, 60)?;
    if pe > 4096 {
        return None;
    }
    let nt = read::<152>(base.checked_add(pe)?)?;
    if &nt[..6] != b"PE\0\0\x64\x86" || &nt[24..26] != b"\x0b\x02" {
        return None;
    }
    let size = word(&nt, 80)?;
    let directory = word(&nt, 136)?;
    let directory_size = word(&nt, 140)?;
    if !(4096..=0x1000_0000).contains(&size)
        || directory == 0
        || directory_size < 40
        || directory.checked_add(directory_size)? > size
    {
        return None;
    }
    let dir = read::<40>(base.checked_add(directory)?)?;
    let dll_at = word(&dir, 12)?;
    if dll_at.checked_add(dll.len())? > size || dll.len() > 32 {
        return None;
    }
    let actual = read::<32>(base.checked_add(dll_at)?)?;
    if !actual[..dll.len()].eq_ignore_ascii_case(dll) {
        return None;
    }
    let functions = word(&dir, 20)?;
    let names = word(&dir, 24)?;
    let ft = word(&dir, 28)?;
    let nt = word(&dir, 32)?;
    let ot = word(&dir, 36)?;
    if functions > 16384
        || names > 16384
        || ft.checked_add(functions * 4)? > size
        || nt.checked_add(names * 4)? > size
        || ot.checked_add(names * 2)? > size
        || name.len() > 64
    {
        return None;
    }
    for index in 0..names {
        let rva = u32::from_le_bytes(read::<4>(base.checked_add(nt + index * 4)?)?) as usize;
        if rva.checked_add(name.len())? > size {
            return None;
        }
        if &read::<64>(base.checked_add(rva)?)?[..name.len()] != name {
            continue;
        }
        let ordinal = u16::from_le_bytes(read::<2>(base.checked_add(ot + index * 2)?)?) as usize;
        if ordinal >= functions {
            return None;
        }
        let address = u32::from_le_bytes(read::<4>(base.checked_add(ft + ordinal * 4)?)?) as usize;
        if address == 0
            || address.checked_add(5)? > size
            || (directory..directory + directory_size).contains(&address)
        {
            return None;
        }
        return base.checked_add(address);
    }
    None
}

fn protection(address: usize, size: usize, anonymous: bool) -> Option<i32> {
    // SAFETY: fixed readonly file; fd and scratch storage are owned below.
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
    let mut total = 0;
    let mut found = None;
    'scan: loop {
        // SAFETY: valid fd and bounded owned output.
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
                let mut fields = line[..length]
                    .split(|b| *b == b' ')
                    .filter(|v| !v.is_empty());
                let bounds = fields.next().unwrap_or_default();
                let perms = fields.next().unwrap_or_default();
                let inode = fields.nth(2).unwrap_or_default();
                if let Some(at) = bounds.iter().position(|b| *b == b'-') {
                    let parse = |text: &[u8]| {
                        usize::from_str_radix(core::str::from_utf8(text).ok()?, 16).ok()
                    };
                    if let (Some(low), Some(high)) =
                        (parse(&bounds[..at]), parse(&bounds[at + 1..]))
                        && low <= address
                        && address.checked_add(size).is_some_and(|v| v <= high)
                        && matches!(perms, b"r-xp" | b"rwxp")
                        && (!anonymous || inode == b"0")
                    {
                        found = Some(
                            libc::PROT_READ
                                | libc::PROT_EXEC
                                | if perms[1] == b'w' {
                                    libc::PROT_WRITE
                                } else {
                                    0
                                },
                        );
                        break 'scan;
                    }
                }
                length = 0;
            } else if length < line.len() {
                line[length] = *byte;
                length += 1;
            } else {
                break 'scan;
            }
        }
    }
    // SAFETY: close only the descriptor owned by this scanner.
    unsafe { libc::close(fd) };
    found
}
fn executable(address: usize, size: usize, anonymous: bool) -> bool {
    protection(address, size, anonymous).is_some()
}

unsafe extern "win64" fn unwind_cleanup(
    _: *mut c_void,
    frame: usize,
    _: *mut c_void,
    _: *mut c_void,
) -> i32 {
    // SAFETY: Wine supplies this registered frame; offset 96 marks an outer invocation.
    if unsafe { ptr::read_unaligned((frame + 96) as *const u32) } != 0 {
        // SAFETY: assembly modifies only this thread's dedicated TLS word.
        unsafe { legion_resolver_clear_depth() };
    }
    1
}
fn register_unwind(observer: &mut Observer) {
    let add = observer
        .ntdll
        .iter()
        .find_map(|base| export(*base, b"ntdll.dll\0", b"RtlAddFunctionTable\0"))
        .unwrap_or_else(|| fail(b"legion-resolver: missing unwind registration\n"));
    // SAFETY: writable POD output for dladdr and an address in this ELF image.
    let mut info: libc::Dl_info = unsafe { core::mem::zeroed() };
    // SAFETY: exact libc query with initialized output.
    if unsafe {
        libc::dladdr(
            legion_resolver_entry as *const () as *const c_void,
            &mut info,
        )
    } == 0
    {
        fail(b"legion-resolver: missing ELF image base\n");
    }
    let base = info.dli_fbase as usize;
    let relative = |at: usize| {
        at.checked_sub(base)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or_else(|| fail(b"legion-resolver: unwind RVA overflow\n"))
    };
    observer.function = [
        relative(legion_resolver_entry as *const () as usize),
        relative(legion_resolver_entry_end as *const () as usize),
        relative(observer.unwind.as_ptr() as usize),
    ];
    observer.unwind[3] = relative(unwind_cleanup as *const () as usize);
    // SAFETY: the validated PE export has the documented Windows ABI; the function and unwind arrays remain mapped and immutable until process exit.
    let add: unsafe extern "win64" fn(*const u32, u32, u64) -> u8 =
        unsafe { core::mem::transmute(add) };
    // SAFETY: complete three-word RUNTIME_FUNCTION and image-relative metadata.
    if unsafe { add(observer.function.as_ptr(), 1, base as u64) } == 0 {
        fail(b"legion-resolver: unwind registration failed\n");
    }
}

fn prepare(observer: &mut Observer) {
    if observer.export == 0 {
        for base in observer.kernel {
            let Some(entry) = export(base, b"kernel32.dll\0", b"GetProcAddress\0") else {
                continue;
            };
            let Some(bytes) = read::<5>(entry) else {
                continue;
            };
            if matches!(bytes[0], 0xe9 | 0xff)
                || bytes[..2] == [0x0f, 0x0b]
                || !executable(entry, 5, false)
            {
                continue;
            }
            observer.export = entry;
            observer.pristine = bytes;
            let message = b"legion-resolver: export observed\n";
            // SAFETY: constant diagnostic bytes, no application contents.
            unsafe { libc::write(2, message.as_ptr().cast(), message.len()) };
            break;
        }
    }
    if observer.export == 0 {
        return;
    }
    let Some(jump) = read::<5>(observer.export) else {
        return;
    };
    if jump[0] != 0xe9 {
        return;
    }
    diagnostic(1, b"legion-resolver: export detour observed\n");
    let displacement = i32::from_le_bytes([jump[1], jump[2], jump[3], jump[4]]);
    let Some(site) = observer
        .export
        .checked_add(5)
        .and_then(|v| v.checked_add_signed(displacement as isize))
    else {
        return;
    };
    if site < 19 || site & 63 > 62 {
        return;
    }
    let Some(original_protection) = protection(site - 19, 33, true) else {
        return;
    };
    diagnostic(2, b"legion-resolver: private executable pair observed\n");
    let Some(pair) = read::<33>(site - 19) else {
        return;
    };
    let Some(target) = validate_pair(observer.export, &observer.pristine, &pair) else {
        return;
    };
    diagnostic(3, b"legion-resolver: gateway pair matched\n");
    if target == site || !executable(target, 1, false) {
        return;
    }
    register_unwind(observer);
    LEGION_RESOLVER_GATEWAY.store(site - 19, Ordering::Release);
    LEGION_RESOLVER_HOOK_POINTER.store(site + 6, Ordering::Release);
    SITE.store(site, Ordering::Release);
    let page = site & !4095;
    // SAFETY: the complete private anonymous RX pair and page were validated.
    if unsafe {
        libc::syscall(
            libc::SYS_mprotect,
            page,
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
        )
    } != 0
    {
        fail(b"legion-resolver: trampoline write access failed\n");
    }
    let mut previous = 0x25ff_u16;
    // SAFETY: validated two-byte word lies within one cache line and page. x86 locked compare-exchange handles unaligned instruction storage without constructing an unaligned Rust atomic reference.
    unsafe {
        core::arch::asm!("lock cmpxchg word ptr [{site}], {new:x}",site=in(reg)site,new=in(reg)0x0b0f_u16,inout("ax")previous,options(nostack));
    }
    // SAFETY: restore this same page's validated original RX or RWX protection.
    if unsafe { libc::syscall(libc::SYS_mprotect, page, 4096, original_protection) } != 0
        || previous != 0x25ff
    {
        fail(b"legion-resolver: trampoline publication failed\n");
    }
    // SAFETY: enable published the complete process-lifetime shared mapping.
    unsafe { &*LEGION_RESOLVER_STATE.load(Ordering::Acquire) }
        .resolver_installed
        .store(1, Ordering::Release);
    let message = b"legion-resolver: pair gate ready\n";
    // SAFETY: constant complete diagnostic bytes.
    unsafe { libc::write(2, message.as_ptr().cast(), message.len()) };
}
fn diagnostic(stage: usize, message: &[u8]) {
    if DIAGNOSTIC_STAGE.fetch_max(stage, Ordering::Relaxed) < stage {
        // SAFETY: complete constant diagnostic, bounded to one write per stage.
        unsafe { libc::write(2, message.as_ptr().cast(), message.len()) };
    }
}
fn validate_pair(export: usize, pristine: &[u8; 5], pair: &[u8; 33]) -> Option<usize> {
    let continuation = u64::from_le_bytes(pair[11..19].try_into().ok()?) as usize;
    let target = u64::from_le_bytes(pair[25..33].try_into().ok()?) as usize;
    (pair[..5] == *pristine
        && pair[5..11] == [0xff, 0x25, 0, 0, 0, 0]
        && pair[19..25] == [0xff, 0x25, 0, 0, 0, 0]
        && continuation == export.checked_add(5)?
        && target != 0)
        .then_some(target)
}

fn observe(address: usize, protection: i32, fd: i32, offset: libc::off_t) {
    if !enabled()
        || SITE.load(Ordering::Acquire) != 0
        || BUSY
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    // SAFETY: this native thread owns errno and BUSY exclusively owns Observer.
    let saved = unsafe { *libc::__errno_location() };
    // SAFETY: BUSY serializes all mutable observer access; published arrays are never modified after SITE becomes nonzero.
    let observer = unsafe { &mut *OBSERVER.0.get() };
    if fd >= 0 && offset == 0 && protection & libc::PROT_READ != 0 {
        let mut link = [0_u8; 64];
        let prefix = b"/proc/self/fd/";
        link[..prefix.len()].copy_from_slice(prefix);
        let mut digits = [0_u8; 10];
        let mut value = fd as u32;
        let mut count = 0;
        loop {
            digits[count] = b'0' + (value % 10) as u8;
            count += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        for i in 0..count {
            link[prefix.len() + i] = digits[count - i - 1];
        }
        let mut path = [0_u8; 4096];
        // SAFETY: complete NUL-terminated path and bounded writable output.
        let used =
            unsafe { libc::readlink(link.as_ptr().cast(), path.as_mut_ptr().cast(), path.len()) };
        if used > 0 && used < path.len() as isize {
            let name = path[..used as usize]
                .rsplit(|b| *b == b'/')
                .next()
                .unwrap_or_default();
            let list = if name.eq_ignore_ascii_case(b"kernel32.dll") {
                Some(&mut observer.kernel)
            } else if name.eq_ignore_ascii_case(b"ntdll.dll") {
                Some(&mut observer.ntdll)
            } else {
                None
            };
            if let Some(list) = list
                && let Some(slot) = list.iter_mut().find(|v| **v == 0 || **v == address)
            {
                *slot = address;
            }
        }
    }
    if protection & libc::PROT_EXEC != 0 {
        prepare(observer);
    }
    BUSY.store(false, Ordering::Release);
    // SAFETY: restore this native caller's errno after observation.
    unsafe { *libc::__errno_location() = saved };
}

/// # Safety Arguments have the native Linux mmap ABI; returned mappings are caller-owned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmap(
    address: *mut c_void,
    length: usize,
    protection: i32,
    flags: i32,
    fd: i32,
    offset: libc::off_t,
) -> *mut c_void {
    // SAFETY: kernel validates the unchanged mmap arguments and returns libc errno.
    let result = unsafe {
        libc::syscall(
            libc::SYS_mmap,
            address,
            length,
            protection,
            flags,
            fd,
            offset,
        )
    } as *mut c_void;
    if result != libc::MAP_FAILED {
        observe(result as usize, protection, fd, offset);
    }
    result
}
/// # Safety Identical to mmap on this x86-64 target (both offsets are 64 bit).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmap64(
    address: *mut c_void,
    length: usize,
    protection: i32,
    flags: i32,
    fd: i32,
    offset: libc::off64_t,
) -> *mut c_void {
    // SAFETY: same arguments and ownership contract as mmap above.
    unsafe { mmap(address, length, protection, flags, fd, offset) }
}
/// # Safety Arguments have the native Linux mprotect ABI and ownership requirements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mprotect(address: *mut c_void, length: usize, protection: i32) -> i32 {
    // SAFETY: kernel validates the caller's unchanged protection request.
    let result = unsafe { libc::syscall(libc::SYS_mprotect, address, length, protection) } as i32;
    if result == 0 {
        observe(address as usize, protection, -1, 0);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_mismatched_gateway_continuation_and_null_hook() {
        let prologue = [0x48, 0x55, 0x48, 0x89, 0xe5];
        let mut pair = [0_u8; 33];
        pair[..5].copy_from_slice(&prologue);
        pair[5..11].copy_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
        pair[19..25].copy_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
        pair[11..19].copy_from_slice(&0x1005_u64.to_le_bytes());
        pair[25..].copy_from_slice(&0x3000_u64.to_le_bytes());
        assert_eq!(validate_pair(0x1000, &prologue, &pair), Some(0x3000));
        for at in [0, 5, 11, 19] {
            let mut bad = pair;
            bad[at] ^= 1;
            assert_eq!(validate_pair(0x1000, &prologue, &bad), None);
        }
        pair[25..].fill(0);
        assert_eq!(validate_pair(0x1000, &prologue, &pair), None);
    }
}
