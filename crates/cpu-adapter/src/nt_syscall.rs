//! Direct Windows SYSCALL boundary only.
use super::{PROCESS, SHARED, fail};
use core::{ffi::c_void, sync::atomic::Ordering};
use legion_cpu_contract::{State, SyscallRoute, syscall};
const _: () = assert!(
    core::mem::size_of::<libc::_libc_fpstate>() == 512
        && core::mem::offset_of!(libc::_libc_fpstate, _xmm) == 160
);

/// # Safety Non-null arguments must be live kernel-provided SA_SIGINFO signal buffers.
pub(super) unsafe fn handle(info: *mut libc::siginfo_t, context: *mut c_void) -> bool {
    if info.is_null() || context.is_null() {
        return false;
    }
    // SIGSYS codes 1 and 2 are Linux seccomp and syscall-user-dispatch traps.
    // SAFETY: caller provides the live kernel signal metadata.
    let code = unsafe { (*info).si_code };
    let pointer = SHARED.load(Ordering::Acquire);
    if pointer.is_null() {
        fail(b"legion-cpu-adapter: SIGSYS missing shared state\n");
    }
    // SAFETY: the constructor retains this aligned mapping until process exit.
    let shared = unsafe { &*pointer };
    if code != 1 && code != 2 {
        shared.nt_unrelated.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    shared.nt_enabled.fetch_or(1_u64 << code, Ordering::Relaxed);
    // SAFETY: valid context for this synchronous x86-64 signal.
    let frame = unsafe { &mut *context.cast::<libc::ucontext_t>() };
    let fp = frame.uc_mcontext.fpregs;
    if fp.is_null() {
        fail(b"legion-cpu-adapter: SIGSYS has no floating-point frame\n");
    }
    // SAFETY: libc's 512-byte legacy x86-64 FPU prefix is present in the live signal frame.
    let xmm5 = unsafe { core::mem::transmute::<[u32; 4], [u64; 2]>((*fp)._xmm[5].element) };
    let g = &mut frame.uc_mcontext.gregs;
    // Cached before interception and refreshed after fork; no syscall here.
    let process = PROCESS.load(Ordering::Acquire);
    let result = syscall(
        State {
            owner: shared.owner.load(Ordering::Acquire),
            declared_process_id: shared.payload.load(Ordering::Acquire),
            handler: shared.handler.load(Ordering::Acquire),
            ready: false,
        },
        process,
        g[libc::REG_RAX as usize] as u64,
        g[libc::REG_RCX as usize] as u64,
        g[libc::REG_R11 as usize] as u64,
        xmm5,
    );
    g[libc::REG_RAX as usize] = result.rax as libc::greg_t;
    g[libc::REG_RCX as usize] = result.rcx as libc::greg_t;
    // SAFETY: writes touch only the source contract's two complete XMM values.
    unsafe {
        (*fp)._xmm[4].element = core::mem::transmute::<[u64; 2], [u32; 4]>(result.xmm4);
        (*fp)._xmm[5].element = core::mem::transmute::<[u64; 2], [u32; 4]>(result.xmm5);
        mark_sse(fp);
    }
    if result.route == SyscallRoute::Handler {
        g[libc::REG_RIP as usize] = result.rcx as libc::greg_t;
        shared.nt_redirected.fetch_add(1, Ordering::Relaxed);
        true
    } else {
        shared.nt_forwarded.fetch_add(1, Ordering::Relaxed);
        false // Wine receives the source-adjusted frame and performs the NT call.
    }
}

/// # Safety `fp` points at a live kernel signal FPU frame, including the declared XSAVE extension if FP_XSTATE_MAGIC1 is present in its legacy reserved bytes.
unsafe fn mark_sse(fp: *mut libc::_libc_fpstate) {
    const MAGIC: u32 = 0x4650_5853;
    let bytes = fp.cast::<u8>();
    // SAFETY: these metadata words lie within the fixed 512-byte legacy prefix.
    let (magic, extent, size) = unsafe {
        (
            bytes.add(464).cast::<u32>().read_unaligned(),
            bytes.add(468).cast::<u32>().read_unaligned(),
            bytes.add(480).cast::<u32>().read_unaligned(),
        )
    };
    if magic == MAGIC {
        if extent < 580 || size < 576 {
            fail(b"legion-cpu-adapter: invalid SIGSYS XSAVE extent\n");
        }
        // SAFETY: kernel metadata above declares the complete XSAVE header.
        unsafe {
            let bv = bytes.add(512).cast::<u64>();
            bv.write_unaligned(bv.read_unaligned() | 2);
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/nt_syscall.rs"]
mod tests;
