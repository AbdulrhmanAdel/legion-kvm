//! Optional owned-Wine readiness worker.
use super::{SHARED, Shared, VENDOR, fail};
use core::{
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};
use legion_cpu_contract::{
    Vendor,
    kuser::{self, PAGE_SIZE},
};

const ADDRESS: usize = 0x7ffe_0000;

pub(super) fn wake(shared: &Shared) {
    // SAFETY: aligned low word of the registration flag; FUTEX_WAKE only observes the mapped address and wakes waiters across the same shared file inode.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            shared.registered.as_ptr().cast::<u32>(),
            libc::FUTEX_WAKE,
            i32::MAX,
        );
    }
}

pub(super) fn start() {
    // SAFETY: fixed name; libc owns the NUL-terminated environment value.
    let enabled = unsafe { libc::getenv(c"LEGION_KUSER_WORKER".as_ptr()) };
    // SAFETY: non-null getenv result is a complete C string.
    if enabled.is_null() || unsafe { libc::strcmp(enabled, c"1".as_ptr()) } != 0 {
        return;
    }
    let shared = SHARED.load(Ordering::Acquire);
    // SAFETY: called after constructor publishes its retained aligned mapping.
    let shared = unsafe { &*shared };
    let _ = shared
        .kuser_status
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
    let mut thread = 0;
    // SAFETY: live output and fixed callback; caller is the normal constructor, not the CPUID signal handler.
    if unsafe { libc::pthread_create(&mut thread, ptr::null(), worker, ptr::null_mut()) } != 0 {
        fail(b"legion-cpu-adapter: KUSER worker creation failed\n");
    }
    // SAFETY: this successfully created worker owns its lifetime until exit.
    if unsafe { libc::pthread_detach(thread) } != 0 {
        fail(b"legion-cpu-adapter: KUSER worker detach failed\n");
    }
}

fn error(shared: &Shared, message: &[u8]) -> ! {
    // SAFETY: the native worker exclusively owns its errno slot.
    shared.kuser_error.store(
        unsafe { *libc::__errno_location() } as u64,
        Ordering::Relaxed,
    );
    shared.kuser_status.store(3, Ordering::Release);
    fail(message)
}

fn is_wine_mapping() -> bool {
    // SAFETY: fixed proc path, descriptor closed below, no process is modified.
    let fd = unsafe {
        libc::open(
            c"/proc/self/maps".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return false;
    }
    let mut chunk = [0_u8; 1024];
    let mut line = [0_u8; 1024];
    let mut length = 0;
    let mut valid = false;
    let mut total = 0;
    'read: loop {
        // SAFETY: valid descriptor and bounded writable scratch buffer.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        total += n as usize;
        if total > 4 * 1024 * 1024 {
            break;
        }
        for &byte in &chunk[..n as usize] {
            if byte == b'\n' {
                let parts = &line[..length];
                if parts.starts_with(b"7ffe0000-") {
                    let mut words = parts.split(|b| *b == b' ').filter(|v| !v.is_empty());
                    let range = words.next().unwrap_or_default();
                    let permissions = words.next().unwrap_or_default();
                    let end = range
                        .get(9..)
                        .and_then(|v| core::str::from_utf8(v).ok())
                        .and_then(|v| usize::from_str_radix(v, 16).ok());
                    let path = parts
                        .iter()
                        .position(|b| *b == b'/')
                        .map(|at| &parts[at..])
                        .unwrap_or_default();
                    // SAFETY: observes this native worker's user identity only.
                    let uid = unsafe { libc::geteuid() };
                    valid = end.is_some_and(|v| v >= ADDRESS + PAGE_SIZE)
                        && matches!(permissions, b"r--s" | b"rw-s")
                        && super::kuser_backing::recognized(path, uid);
                    if !valid {
                        // SAFETY: bounded checked mapping line, contains no process contents; only diagnoses this fixed ABI page.
                        unsafe {
                            libc::write(2, parts.as_ptr().cast(), parts.len());
                            libc::write(2, b"\n".as_ptr().cast(), 1);
                        }
                    }
                    break 'read;
                }
                length = 0;
            } else if length < line.len() {
                line[length] = byte;
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
    valid
}

/// # Safety Both mappings contain a complete KUSER page and remain live.
unsafe fn clocks(source: *const u8, target: *mut u8) {
    for offset in [0x8, 0x14, 0x320] {
        let mut stable = None;
        for _ in 0..100 {
            // SAFETY: KSYSTEM_TIME is three aligned u32 words within the page.
            let (high, low, other) = unsafe {
                (
                    source.add(offset + 4).cast::<u32>().read_volatile(),
                    source.add(offset).cast::<u32>().read_volatile(),
                    source.add(offset + 8).cast::<u32>().read_volatile(),
                )
            };
            if high == other {
                stable = Some((low, high));
                break;
            }
        }
        if let Some((low, high)) = stable {
            // SAFETY: aligned integer atomics in the private shared page.
            unsafe {
                (&*target.add(offset + 8).cast::<AtomicU32>()).store(high, Ordering::SeqCst);
                (&*target.add(offset).cast::<AtomicU32>()).store(low, Ordering::SeqCst);
                (&*target.add(offset + 4).cast::<AtomicU32>()).store(high, Ordering::SeqCst);
            }
        }
    }

    // SAFETY: these u64 fields are page-contained and 8-byte aligned.
    unsafe {
        (&*target.add(0x340).cast::<AtomicU64>()).store(
            source.add(0x340).cast::<u64>().read_volatile(),
            Ordering::Release,
        );
        let baseline = source.add(0x348).cast::<u64>().read_volatile();
        for to in [0x348, 0x350] {
            (&*target.add(to).cast::<AtomicU64>()).store(baseline, Ordering::Release);
        }
    }
    // SAFETY: the last-input tick is a page-contained aligned u32.
    unsafe {
        (&*target.add(0x2e4).cast::<AtomicU32>()).store(
            source.add(0x2e4).cast::<u32>().read_volatile(),
            Ordering::Relaxed,
        );
    }
}

extern "C" fn worker(_: *mut c_void) -> *mut c_void {
    // This is a native housekeeping thread without a Wine TEB.
    // SAFETY: valid initialized POSIX set, affects only this worker's mask.
    unsafe {
        let mut mask: libc::sigset_t = core::mem::zeroed();
        libc::sigfillset(&mut mask);
        libc::sigdelset(&mut mask, libc::SIGSEGV);
        libc::pthread_sigmask(libc::SIG_SETMASK, &mask, ptr::null_mut());
    }
    // SAFETY: constructor starts this only after publishing the retained state.
    let shared = unsafe { &*SHARED.load(Ordering::Acquire) };
    // SAFETY: only observes this worker's Linux thread group.
    let process = unsafe { libc::getpid() } as u64;
    loop {
        if shared.registered.load(Ordering::Acquire) != 0 {
            let owner = shared.owner.load(Ordering::Acquire);
            if owner != process {
                return ptr::null_mut();
            }

            if shared.payload.load(Ordering::Acquire) == 0 {
                return ptr::null_mut();
            }
            break;
        }
        // SAFETY: aligned shared futex word, expected zero.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                shared.registered.as_ptr().cast::<u32>(),
                libc::FUTEX_WAIT,
                0,
                ptr::null::<libc::timespec>(),
            );
        }
    }
    if !is_wine_mapping() {
        error(
            shared,
            b"legion-cpu-adapter: unrecognized Wine KUSER mapping\n",
        );
    }
    // SAFETY: verified MAP_SHARED page; old_size=0 duplicates its backing pages without moving/unmapping the original.
    let source =
        unsafe { libc::mremap(ADDRESS as *mut c_void, 0, PAGE_SIZE, libc::MREMAP_MAYMOVE) };
    if source == libc::MAP_FAILED {
        error(shared, b"legion-cpu-adapter: KUSER source alias failed\n");
    }
    // SAFETY: new private backing object, never shared with another process.
    let fd = unsafe { libc::memfd_create(c"legion-kuser-private".as_ptr(), libc::MFD_CLOEXEC) };
    // SAFETY: successful fd admits a fixed page size.
    if fd < 0 || unsafe { libc::ftruncate(fd, PAGE_SIZE as libc::off_t) } != 0 {
        error(
            shared,
            b"legion-cpu-adapter: KUSER backing allocation failed\n",
        );
    }
    // SAFETY: new page has no observers until the fixed readonly alias is published.
    let private = unsafe {
        libc::mmap(
            ptr::null_mut(),
            PAGE_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if private == libc::MAP_FAILED {
        error(shared, b"legion-cpu-adapter: KUSER writable alias failed\n");
    }
    let source = source.cast::<u8>();
    let private = private.cast::<u8>();
    for offset in 0..PAGE_SIZE {
        // SAFETY: retained native source and exclusively owned new destination.
        unsafe {
            private
                .add(offset)
                .write(source.add(offset).read_volatile());
        }
    }
    let vendor = if VENDOR.load(Ordering::Relaxed) == 1 {
        Vendor::Intel
    } else {
        Vendor::Amd
    };
    // SAFETY: initialized aligned page; no other alias has been published yet.
    kuser::apply(vendor, unsafe { &mut *private.cast::<[u8; PAGE_SIZE]>() });
    // SAFETY: both complete mappings remain live until process exit.
    unsafe {
        clocks(source, private);
    }
    // SAFETY: replace only the verified fixed KUSER page in this process.
    let fixed = unsafe {
        libc::mmap(
            ADDRESS as *mut c_void,
            PAGE_SIZE,
            libc::PROT_READ,
            libc::MAP_SHARED | libc::MAP_FIXED,
            fd,
            0,
        )
    };
    // SAFETY: both aliases retain the backing after its descriptor is closed.
    unsafe {
        libc::close(fd);
    }
    if fixed != ADDRESS as *mut c_void {
        error(
            shared,
            b"legion-cpu-adapter: KUSER readonly publication failed\n",
        );
    }
    super::wine_dispatch::install(shared);
    shared.kuser_updates.store(1, Ordering::Relaxed);
    shared.kuser_status.store(2, Ordering::Release);
    shared.ready.store(1, Ordering::Release);
    let interval = libc::timespec {
        tv_sec: 0,
        tv_nsec: 1_000_000,
    };
    loop {
        // SAFETY: relative native sleep and retained complete source/target pages.
        unsafe {
            libc::nanosleep(&interval, ptr::null_mut());
            clocks(source, private);
        }
        shared.kuser_updates.fetch_add(1, Ordering::Relaxed);
    }
}
