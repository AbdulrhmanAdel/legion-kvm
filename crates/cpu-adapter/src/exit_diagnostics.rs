//! Opt-in native thread-exit observation in the owned guest.
use super::{PROCESS, fail};
use core::{
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
static ENABLED: AtomicBool = AtomicBool::new(false);
static REAL: AtomicUsize = AtomicUsize::new(0);

pub(super) fn initialize() {
    // SAFETY: getenv returns null or a readable NUL-terminated process string.
    let value = unsafe { libc::getenv(c"LEGION_EXIT_DIAGNOSTICS".as_ptr()) };
    // SAFETY: the non-null environment value is complete.
    if !value.is_null() && unsafe { libc::strcmp(value, c"1".as_ptr()) } == 0 {
        ENABLED.store(true, Ordering::Release);
    }
}

fn hex(out: &mut [u8], at: &mut usize, value: usize) {
    for shift in (0..16).rev() {
        out[*at] = b"0123456789abcdef"[(value >> (shift * 4)) & 15];
        *at += 1;
    }
    out[*at] = b' ';
    *at += 1;
}

/// # Safety Same pthread_exit ABI and caller obligations as libc.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_exit(_value: *mut c_void) -> ! {
    // Tail-enter libc with the original stack. pthread_exit may force-unwind; no Rust frame may remain between it and the native caller.
    core::arch::naked_asm!(
        ".cfi_startproc",
        "push rdi",
        ".cfi_def_cfa_offset 16",
        "call legion_exit_route",
        "pop rdi",
        ".cfi_def_cfa_offset 8",
        "jmp rax",
        ".cfi_endproc"
    );
}

/// # Safety Invoked by the native assembly shim with pthread_exit's original argument.
#[unsafe(no_mangle)]
unsafe extern "C" fn legion_exit_route(value: *mut c_void) -> usize {
    let mut address = REAL.load(Ordering::Acquire);
    if address == 0 {
        // SAFETY: resolve libc's exact POSIX function after this preload.
        address = unsafe { libc::dlsym(libc::RTLD_NEXT, c"pthread_exit".as_ptr()) } as usize;
        if address == 0 {
            fail(b"legion-exit: missing native pthread_exit\n");
        }
        REAL.store(address, Ordering::Release);
    }
    if ENABLED.load(Ordering::Acquire)
        // SAFETY: gettid has no pointer arguments or side effects.
        && unsafe{libc::syscall(libc::SYS_gettid)} as u64==PROCESS.load(Ordering::Acquire)
    {
        let mut frames = [ptr::null_mut::<c_void>(); 32];
        // SAFETY: this is a normal native function call, not a signal handler; libc receives the complete initialized output array and capacity.
        let count =
            unsafe { libc::backtrace(frames.as_mut_ptr(), frames.len() as i32) }.max(0) as usize;
        let mut bytes = [0_u8; 640];
        let prefix = b"legion-exit: main pthread_exit value/backtrace ";
        bytes[..prefix.len()].copy_from_slice(prefix);
        let mut used = prefix.len();
        hex(&mut bytes, &mut used, value as usize);
        for frame in &frames[..count.min(frames.len())] {
            hex(&mut bytes, &mut used, *frame as usize);
        }
        bytes[used] = b'\n';
        used += 1;
        // SAFETY: the formatted code addresses occupy this bounded owned slice.
        unsafe { libc::write(2, bytes.as_ptr().cast(), used) };
    }
    address
}
