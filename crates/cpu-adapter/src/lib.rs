#![no_std]
use core::{
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering},
};
use legion_cpu_contract::{
    Context, ControlRequest, Privilege, Registers, State, Vendor, control_request,
};

const MAGIC: u64 = u64::from_le_bytes(*b"LCPID001");
const QUERY_NATIVE: libc::c_ulong = 0xc028_4c01;
const SET_FAULT: libc::c_ulong = 0x4004_4c02;
const GET_CAPS: libc::c_ulong = 0x8010_4c00;
#[cfg(feature = "exit-diagnostics")]
mod exit_diagnostics;
mod kuser_backing;
mod kuser_worker;
mod nt_syscall;
mod resolver;
mod wine_dispatch;
#[repr(C)]
struct Query {
    ip: u64,
    leaf: u32,
    subleaf: u32,
    output: [u32; 4],
    length: u32,
    reserved: u32,
}
#[repr(C, align(64))]
struct Shared {
    magic: AtomicU64,
    owner: AtomicU64,
    payload: AtomicU64,
    handler: AtomicU64,
    ready: AtomicU64,
    queries: AtomicU64,
    failures: AtomicU64,
    delegated: AtomicU64,
    kuser_status: AtomicU64,
    kuser_updates: AtomicU64,
    kuser_error: AtomicU64,
    registered: AtomicU64,
    nt_redirected: AtomicU64,
    nt_forwarded: AtomicU64,
    nt_unrelated: AtomicU64,
    nt_enabled: AtomicU64,
    dispatch_installed: AtomicU64,
    dispatch_redirected: AtomicU64,
    dispatch_forwarded: AtomicU64,
    resolver_installed: AtomicU64,
    resolver_outer: AtomicU64,
    resolver_nested: AtomicU64,
}
const _: () = assert!(core::mem::size_of::<Query>() == 40 && core::mem::size_of::<Shared>() == 192);
type Action = libc::sigaction;
type RealAction = unsafe extern "C" fn(i32, *const Action, *mut Action) -> i32;
static REAL: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static WRITER: AtomicBool = AtomicBool::new(false);
static ORIGINAL: AtomicPtr<Action> = AtomicPtr::new(ptr::null_mut());
static ORIGINAL_SYS: AtomicPtr<Action> = AtomicPtr::new(ptr::null_mut());
static ORIGINAL_ILL: AtomicPtr<Action> = AtomicPtr::new(ptr::null_mut());
static NT_ENABLED: AtomicBool = AtomicBool::new(false);
static SHARED: AtomicPtr<Shared> = AtomicPtr::new(ptr::null_mut());
static DRIVER: AtomicI32 = AtomicI32::new(-1);
static VENDOR: AtomicU8 = AtomicU8::new(0);
static PROCESS: AtomicU64 = AtomicU64::new(0);

fn fail(message: &[u8]) -> ! {
    let shared = SHARED.load(Ordering::Acquire);
    if !shared.is_null() {
        // SAFETY: published mappings remain valid until process exit.
        unsafe { &*shared }.failures.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: constant/readable message buffer; terminate only this process.
    unsafe {
        libc::write(2, message.as_ptr().cast(), message.len());
        libc::_exit(193)
    }
}

fn real_action() -> RealAction {
    let mut address = REAL.load(Ordering::Acquire);
    if address == 0 {
        // SAFETY: fixed symbol name; RTLD_NEXT resolves libc/next interposer.
        address = unsafe { libc::dlsym(libc::RTLD_NEXT, c"sigaction".as_ptr()) } as usize;
        if address == 0 {
            fail(b"legion-cpu-adapter: missing real sigaction\n");
        }
        REAL.store(address, Ordering::Release);
    }
    // SAFETY: non-null address was resolved by the exact POSIX function name.
    unsafe { core::mem::transmute::<usize, RealAction>(address) }
}

fn guest() -> bool {
    // The root bootstrap verifies /proc/1/exe before enabling this option.
    // SAFETY: constant readonly proc path; descriptor is closed below.
    let fd = unsafe { libc::open(c"/proc/cmdline".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return false;
    }
    let mut bytes = [0_u8; 4096];
    // SAFETY: bounded writable buffer; fd is valid and uniquely owned here.
    let count = unsafe {
        let n = libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len());
        libc::close(fd);
        n
    };
    count > 0
        && bytes[..count as usize]
            .split(u8::is_ascii_whitespace)
            .any(|v| v == b"legion.vulkan=1")
}

fn open_driver() -> i32 {
    // SAFETY: fixed guest device path; retained fd is valid until process exit.
    let fd = unsafe {
        libc::open(
            c"/dev/legion_cpuid".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        fail(b"legion-cpu-adapter: guest CPUID device unavailable\n");
    }
    DRIVER.store(fd, Ordering::Release);
    fd
}

fn native(query: &mut Query) -> bool {
    let mut fd = DRIVER.load(Ordering::Acquire);
    if fd < 0 {
        fd = open_driver();
    }
    // SAFETY: matching guest ioctl ABI and initialized writable Query.
    if unsafe { libc::ioctl(fd, QUERY_NATIVE, query as *mut Query) } == 0 {
        return true;
    }
    // A fork/posix_spawn child may close inherited descriptors before exec.
    // SAFETY: errno is this native thread's libc error slot.
    let error = unsafe { *libc::__errno_location() };
    if error != libc::EBADF && error != libc::ENOTTY {
        return false;
    }
    fd = open_driver();
    // SAFETY: same checked ioctl ABI and output lifetime as above.
    unsafe { libc::ioctl(fd, QUERY_NATIVE, query as *mut Query) == 0 }
}

fn snapshot(action: Action) -> *mut Action {
    // SAFETY: fresh private page, suitably aligned and large enough for Action.
    let page = unsafe {
        libc::mmap(
            ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if page == libc::MAP_FAILED {
        fail(b"legion-cpu-adapter: signal snapshot allocation failed\n");
    }
    // SAFETY: fresh mapping is writable/aligned; Action is an initialized C POD.
    unsafe {
        page.cast::<Action>().write(action);
    }
    page.cast()
}

unsafe extern "C" fn bridge(signal: i32, info: *mut libc::siginfo_t, context: *mut c_void) {
    // SAFETY: libc errno is the current native thread's live error slot.
    let saved_errno = unsafe { *libc::__errno_location() };
    if signal == libc::SIGILL && resolver::enabled()
        // SAFETY: the kernel supplies the live SA_SIGINFO buffers.
        && unsafe { resolver::handle(info, context) }
    {
        // SAFETY: restore this interrupted thread's native errno.
        unsafe { *libc::__errno_location() = saved_errno };
        return;
    }
    if signal == libc::SIGSYS && NT_ENABLED.load(Ordering::Acquire) {
        // SAFETY: the kernel supplies live signal buffers to this bridge.
        if unsafe { nt_syscall::handle(info, context) } {
            // SAFETY: restore this interrupted thread's native error value.
            unsafe {
                *libc::__errno_location() = saved_errno;
            }
            return;
        }
    }
    if signal == libc::SIGSEGV && !info.is_null() && !context.is_null() {
        // SAFETY: kernel supplies live siginfo/ucontext for this SA_SIGINFO handler.
        let (code, frame) = unsafe { ((*info).si_code, &mut *context.cast::<libc::ucontext_t>()) };
        let g = &mut frame.uc_mcontext.gregs;
        if code == libc::SI_KERNEL && g[libc::REG_TRAPNO as usize] == 13 {
            let input = [
                g[libc::REG_RAX as usize] as u64,
                g[libc::REG_RBX as usize] as u64,
                g[libc::REG_RCX as usize] as u64,
                g[libc::REG_RDX as usize] as u64,
            ];
            let mut query = Query {
                ip: g[libc::REG_RIP as usize] as u64,
                leaf: input[0] as u32,
                subleaf: input[2] as u32,
                output: [0; 4],
                length: 0,
                reserved: 0,
            };
            if native(&mut query) {
                let pointer = SHARED.load(Ordering::Acquire);
                if pointer.is_null() {
                    fail(b"legion-cpu-adapter: missing shared state\n");
                }
                // SAFETY: constructor mapped a complete aligned Shared for process lifetime.
                let shared = unsafe { &*pointer };
                // Cached before interception and refreshed after fork.
                let process = PROCESS.load(Ordering::Acquire);
                let vendor = if VENDOR.load(Ordering::Relaxed) == 1 {
                    Vendor::Intel
                } else {
                    Vendor::Amd
                };
                let request = control_request(vendor, input[0]);
                if request == ControlRequest::Register
                    && shared
                        .owner
                        .compare_exchange(0, process, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    shared.payload.store(input[3], Ordering::Release);
                    shared.registered.store(1, Ordering::Release);
                    kuser_worker::wake(shared);
                }
                let owner = shared.owner.load(Ordering::Acquire);
                if owner == process && request == ControlRequest::Handler {
                    let _ = shared.handler.compare_exchange(
                        0,
                        input[2],
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
                let ready = owner == process
                    && request == ControlRequest::Ready
                    && shared.ready.swap(0, Ordering::AcqRel) != 0;
                let mut state = State {
                    owner,
                    declared_process_id: shared.payload.load(Ordering::Acquire),
                    handler: shared.handler.load(Ordering::Acquire),
                    ready,
                };
                let reply = state.cpuid(
                    vendor,
                    Context {
                        process,
                        privilege: Privilege::User,
                        stack_privilege: Privilege::User,
                    },
                    input,
                    Registers::new(
                        query.output[0],
                        query.output[1],
                        query.output[2],
                        query.output[3],
                    ),
                );
                for (index, value) in [libc::REG_RAX, libc::REG_RBX, libc::REG_RCX, libc::REG_RDX]
                    .into_iter()
                    .zip(reply.registers)
                {
                    g[index as usize] = value as libc::greg_t;
                }
                g[libc::REG_RIP as usize] += i64::from(query.length);
                shared.queries.fetch_add(1, Ordering::Relaxed);
                // SAFETY: CPUID must not publish the adapter's libc errors.
                unsafe {
                    *libc::__errno_location() = saved_errno;
                }
                return;
            }
            // ENXIO means a different #GP instruction.
            // SAFETY: this thread's errno follows the failed synchronous ioctl.
            let error = unsafe { *libc::__errno_location() };
            if error != libc::ENXIO && error != libc::EFAULT {
                fail(b"legion-cpu-adapter: native CPUID query failed\n");
            }
        }
    }
    // SAFETY: delegate with the original interrupted thread's errno value.
    unsafe {
        *libc::__errno_location() = saved_errno;
    }
    let original = action_slot(signal).load(Ordering::Acquire);
    if original.is_null() {
        fail(b"legion-cpu-adapter: missing signal delegate\n");
    }
    // SAFETY: immutable snapshot retained for the lifetime of this handler.
    let action = unsafe { &*original };
    if action.sa_sigaction == libc::SIG_IGN {
        return;
    }
    if action.sa_sigaction == libc::SIG_DFL {
        // SAFETY: restore original disposition and requeue only this thread's signal.
        unsafe {
            real_action()(signal, original, ptr::null_mut());
            libc::syscall(
                libc::SYS_tgkill,
                libc::getpid(),
                libc::syscall(libc::SYS_gettid),
                signal,
            );
        }
        return;
    }
    let shared = SHARED.load(Ordering::Relaxed);
    if !shared.is_null() {
        // SAFETY: same process-lifetime mapping as above.
        unsafe { &*shared }
            .delegated
            .fetch_add(1, Ordering::Relaxed);
    }
    if action.sa_flags & libc::SA_SIGINFO != 0 {
        // SAFETY: original SA_SIGINFO disposition has this exact POSIX ABI.
        let handler = unsafe {
            core::mem::transmute::<
                usize,
                unsafe extern "C" fn(i32, *mut libc::siginfo_t, *mut c_void),
            >(action.sa_sigaction)
        };
        // SAFETY: forward the unchanged live signal frame to its registered owner.
        unsafe {
            handler(signal, info, context);
        }
    } else {
        // SAFETY: original non-SA_SIGINFO disposition takes one signal argument.
        let handler = unsafe {
            core::mem::transmute::<usize, unsafe extern "C" fn(i32)>(action.sa_sigaction)
        };
        // SAFETY: invoking the saved registered signal disposition.
        unsafe {
            handler(signal);
        }
    }
}

fn action_slot(signal: i32) -> &'static AtomicPtr<Action> {
    match signal {
        libc::SIGSYS => &ORIGINAL_SYS,
        libc::SIGILL => &ORIGINAL_ILL,
        _ => &ORIGINAL,
    }
}

/// # Safety
/// Pointer arguments follow POSIX sigaction's readable/writable buffer contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigaction(signal: i32, action: *const Action, old: *mut Action) -> i32 {
    let real = real_action();
    if !ACTIVE.load(Ordering::Acquire)
        || !(signal == libc::SIGSEGV
            || (signal == libc::SIGILL && resolver::enabled())
            || (signal == libc::SIGSYS && NT_ENABLED.load(Ordering::Acquire)))
    {
        // SAFETY: unchanged caller arguments passed to the actual libc function.
        return unsafe { real(signal, action, old) };
    }
    // SAFETY: C signal set structs admit all-zero initialization.
    let (mut blocked, mut previous) = unsafe {
        (
            core::mem::zeroed::<libc::sigset_t>(),
            core::mem::zeroed::<libc::sigset_t>(),
        )
    };
    // SAFETY: initialized writable sets; block catchable signals during the short publication critical section, then restore the caller's mask.
    unsafe {
        libc::sigfillset(&mut blocked);
        libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous);
    }
    while WRITER
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    let slot = action_slot(signal);
    let before = slot.load(Ordering::Acquire);
    let mut result = 0;
    if !action.is_null() {
        // SAFETY: caller supplies a readable Action by the public FFI contract.
        let requested = unsafe { ptr::read(action) };
        if requested.sa_flags & libc::SA_RESETHAND != 0 {
            // SAFETY: errno belongs to this native thread.
            unsafe {
                *libc::__errno_location() = libc::ENOTSUP;
            }
            result = -1;
        } else {
            let mut installed = requested;
            installed.sa_sigaction = bridge as *const () as usize;
            installed.sa_flags |= libc::SA_SIGINFO;
            // SAFETY: complete initialized kernel action and checked libc function.
            result = unsafe { real(signal, &installed, ptr::null_mut()) };
            if result == 0 {
                // SAFETY: initialized C POD output lives through synchronous query.
                let mut normalized: Action = unsafe { core::mem::zeroed() };
                // SAFETY: valid output, null input requests current disposition.
                if unsafe { real(signal, ptr::null(), &mut normalized) } != 0 {
                    fail(b"legion-cpu-adapter: disposition query failed\n");
                }
                normalized.sa_sigaction = requested.sa_sigaction;
                normalized.sa_flags = (normalized.sa_flags & !libc::SA_SIGINFO)
                    | (requested.sa_flags & libc::SA_SIGINFO);
                slot.store(snapshot(normalized), Ordering::Release);
            }
        }
    }
    if result == 0 && !old.is_null() && !before.is_null() {
        // SAFETY: old is caller-owned writable; before is retained immutable data.
        unsafe {
            old.write(ptr::read(before));
        }
    }
    WRITER.store(false, Ordering::Release);
    // SAFETY: restore the exact previously observed native mask.
    unsafe {
        libc::pthread_sigmask(libc::SIG_SETMASK, &previous, ptr::null_mut());
    }
    result
}

unsafe extern "C" fn after_fork() {
    WRITER.store(false, Ordering::Release);
    // SAFETY: POSIX getpid is safe in this native fork child callback.
    PROCESS.store(unsafe { libc::getpid() } as u64, Ordering::Release);
}

extern "C" fn initialize() {
    let _ = real_action();
    // SAFETY: getenv returns a process-owned NUL-terminated value or null.
    let enabled = unsafe { libc::getenv(c"LEGION_CPU_ADAPTER".as_ptr()) };
    // SAFETY: non-null getenv result is readable through its terminating NUL.
    if enabled.is_null() || unsafe { libc::strcmp(enabled, c"1".as_ptr()) } != 0 || !guest() {
        return;
    }
    // SAFETY: cache ownership before enabling either interception path.
    PROCESS.store(unsafe { libc::getpid() } as u64, Ordering::Release);
    #[cfg(feature = "exit-diagnostics")]
    exit_diagnostics::initialize();
    let fd = open_driver();
    let mut caps = [0_u32; 4];
    // SAFETY: exact 16-byte capability output for this guest module ABI.
    if unsafe { libc::ioctl(fd, GET_CAPS, caps.as_mut_ptr()) } != 0
        || caps[0] != 1
        || caps[1] != 1
        || caps[2] == 0
        || caps[3] != 0
    {
        fail(b"legion-cpu-adapter: guest driver ABI mismatch\n");
    }
    let mut query = Query {
        ip: 0,
        leaf: 0,
        subleaf: 0,
        output: [0; 4],
        length: 0,
        reserved: 0,
    };
    if !native(&mut query) {
        fail(b"legion-cpu-adapter: native query unsupported\n");
    }
    let vendor = match (query.output[1], query.output[3], query.output[2]) {
        (0x68747541, 0x69746e65, 0x444d4163) => 0,
        (0x756e6547, 0x49656e69, 0x6c65746e) => 1,
        _ => fail(b"legion-cpu-adapter: unsupported CPU vendor\n"),
    };
    VENDOR.store(vendor, Ordering::Relaxed);
    // SAFETY: fixed per-run file, no symlink following; bootstrap owns creation.
    let state_fd = unsafe {
        libc::open(
            c"/frames/cpu-adapter-state.bin".as_ptr(),
            libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if state_fd < 0 {
        fail(b"legion-cpu-adapter: missing state file\n");
    }
    // SAFETY: writable POD metadata output and live descriptor.
    let mut stat: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: fstat only observes the live fd into initialized output.
    if unsafe { libc::fstat(state_fd, &mut stat) } != 0
        || stat.st_size != 4096
        || stat.st_mode & libc::S_IFMT != libc::S_IFREG
    {
        fail(b"legion-cpu-adapter: invalid state file\n");
    }
    // SAFETY: file is 4096 bytes, mapping is aligned, shared and retained until exit.
    let mapping = unsafe {
        libc::mmap(
            ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            state_fd,
            0,
        )
    };
    // SAFETY: this descriptor is no longer needed after mmap.
    unsafe {
        libc::close(state_fd);
    }
    if mapping == libc::MAP_FAILED {
        fail(b"legion-cpu-adapter: state mapping failed\n");
    }
    // SAFETY: page-aligned zero-created mapping admits atomic integer fields.
    let state = unsafe { &*mapping.cast::<Shared>() };
    if let Err(found) = state
        .magic
        .compare_exchange(0, MAGIC, Ordering::AcqRel, Ordering::Acquire)
        && found != MAGIC
    {
        fail(b"legion-cpu-adapter: state ABI mismatch\n");
    }
    SHARED.store(mapping.cast(), Ordering::Release);
    // SAFETY: signal action POD starts initialized; handler is valid process code.
    let (mut action, mut previous): (Action, Action) = unsafe { core::mem::zeroed() };
    action.sa_sigaction = bridge as *const () as usize;
    action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
    // SAFETY: empty mask and synchronous real sigaction install while CPUID is allowed.
    if unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        real_action()(libc::SIGSEGV, &action, &mut previous)
    } != 0
    {
        fail(b"legion-cpu-adapter: signal bridge installation failed\n");
    }
    ORIGINAL.store(snapshot(previous), Ordering::Release);
    // SAFETY: fixed environment string, checked before any SIGSYS interception.
    let nt = unsafe { libc::getenv(c"LEGION_NT_SYSCALL".as_ptr()) };
    // SAFETY: a non-null getenv value is a complete C string.
    if !nt.is_null() && unsafe { libc::strcmp(nt, c"1".as_ptr()) } == 0 {
        // SAFETY: initialized C POD output, fixed valid handler and real libc.
        if unsafe { real_action()(libc::SIGSYS, &action, &mut previous) } != 0 {
            fail(b"legion-cpu-adapter: SIGSYS bridge installation failed\n");
        }
        ORIGINAL_SYS.store(snapshot(previous), Ordering::Release);
        NT_ENABLED.store(true, Ordering::Release);
        state.nt_enabled.fetch_or(1, Ordering::Release);
    }
    if resolver::requested() {
        // SAFETY: install a separate SIGILL delegate before enabling observation.
        if unsafe { real_action()(libc::SIGILL, &action, &mut previous) } != 0 {
            fail(b"legion-cpu-adapter: SIGILL bridge installation failed\n");
        }
        ORIGINAL_ILL.store(snapshot(previous), Ordering::Release);
        resolver::enable(state);
    }
    // SAFETY: no prepare/parent callbacks; the child callback only resets its process-private publication lock after a multithreaded fork.
    if unsafe { libc::pthread_atfork(None, None, Some(after_fork)) } != 0 {
        fail(b"legion-cpu-adapter: fork callback failed\n");
    }
    ACTIVE.store(true, Ordering::Release);
    let mode = 1_u32;
    // SAFETY: matching task-local guest ioctl with readable initialized input.
    if unsafe { libc::ioctl(fd, SET_FAULT, &mode) } != 0 {
        fail(b"legion-cpu-adapter: task fault activation failed\n");
    }
    kuser_worker::start();
}

#[used]
#[unsafe(link_section = ".init_array")]
static INITIALIZE: extern "C" fn() = initialize;

#[cfg(not(test))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    fail(b"legion-cpu-adapter: invariant failure\n")
}
