use legion_cpu_contract::{Context, Privilege, Registers, State, Vendor};
use std::{
    io, ptr,
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
};

static SIGNALS: AtomicU64 = AtomicU64::new(0);
static HANDLER_ERRORS: AtomicU64 = AtomicU64::new(0);
static SYS_SIGNALS: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn sys_signal(signal: i32, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // SAFETY: kernel-provided live SA_SIGINFO metadata, checked before reading.
    if signal == libc::SIGSYS && !info.is_null() && unsafe { (*info).si_code } == libc::SI_TKILL {
        SYS_SIGNALS.fetch_add(1, Ordering::Relaxed);
    } else {
        HANDLER_ERRORS.fetch_add(1, Ordering::Relaxed);
    }
}

fn sys_signal_contract() -> io::Result<()> {
    if std::env::var_os("LEGION_NT_SYSCALL").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return Ok(());
    }
    // SAFETY: POSIX C signal dispositions admit these zero-initialized values.
    let (mut action, mut original, mut observed): (
        libc::sigaction,
        libc::sigaction,
        libc::sigaction,
    ) = unsafe { std::mem::zeroed() };
    action.sa_sigaction = sys_signal as *const () as usize;
    action.sa_flags = libc::SA_SIGINFO;
    // SAFETY: complete live signal structures for this owned disposition.
    if unsafe { libc::sigaction(libc::SIGSYS, &action, &mut original) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: requests the current disposition into live writable output.
    if unsafe { libc::sigaction(libc::SIGSYS, ptr::null(), &mut observed) } != 0 {
        return Err(io::Error::last_os_error());
    }
    assert_eq!(observed.sa_sigaction, action.sa_sigaction);
    // SAFETY: raises only this fixture's signal, handled by its own callback.
    assert_eq!(unsafe { libc::raise(libc::SIGSYS) }, 0);
    assert_eq!(SYS_SIGNALS.load(Ordering::Relaxed), 1);
    assert_eq!(HANDLER_ERRORS.load(Ordering::Relaxed), 0);
    // SAFETY: restores exactly the previous owned signal disposition.
    if unsafe { libc::sigaction(libc::SIGSYS, &original, ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    println!("LEGION_SIGSYS_DELEGATION_PASSED");
    Ok(())
}

fn cpuid(mut values: [u64; 4]) -> [u64; 4] {
    // SAFETY: CPUID is intercepted in this opt-in guest; RBX is restored and all full-width outputs are declared.
    unsafe {
        core::arch::asm!("xchg {saved}, rbx", "cpuid", "xchg {saved}, rbx",
        saved = inout(reg) values[1], inout("rax") values[0],
        inout("rcx") values[2], inout("rdx") values[3], options(nostack,preserves_flags));
    }
    values
}

fn native(leaf: u32, subleaf: u32) -> io::Result<[u64; 4]> {
    #[repr(C)]
    struct Query {
        ip: u64,
        leaf: u32,
        subleaf: u32,
        output: [u32; 4],
        length: u32,
        reserved: u32,
    }
    // SAFETY: fixed guest-only device path.
    let fd = unsafe {
        libc::open(
            c"/dev/legion_cpuid".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut query = Query {
        ip: 0,
        leaf,
        subleaf,
        output: [0; 4],
        length: 0,
        reserved: 0,
    };
    // SAFETY: exact 40-byte native query ABI with writable output.
    let result = unsafe { libc::ioctl(fd, 0xc028_4c01 as libc::c_ulong, &mut query) };
    let error = io::Error::last_os_error();
    // SAFETY: fd is owned by this call.
    unsafe {
        libc::close(fd);
    }
    if result != 0 {
        return Err(error);
    }
    Ok(query.output.map(u64::from))
}

unsafe extern "C" fn signal(signal: i32, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    if signal != libc::SIGSEGV || info.is_null() || context.is_null() {
        HANDLER_ERRORS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: the kernel supplies valid live siginfo/ucontext for SA_SIGINFO.
    let (info, frame) = unsafe { (&*info, &mut *context.cast::<libc::ucontext_t>()) };
    if info.si_code == libc::SI_TKILL {
        SIGNALS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if frame.uc_mcontext.gregs[libc::REG_TRAPNO as usize] == 13 {
        // SAFETY: RIP points at this fixture's live instruction text.
        let opcode = unsafe { *(frame.uc_mcontext.gregs[libc::REG_RIP as usize] as *const u8) };
        if opcode == 0xed {
            frame.uc_mcontext.gregs[libc::REG_RIP as usize] += 1;
            SIGNALS.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    HANDLER_ERRORS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: an unexpected synchronous fault must fail the owned fixture.
    unsafe {
        libc::_exit(92);
    }
}

fn signal_contract() -> io::Result<()> {
    // SAFETY: zero initializes valid POSIX C POD structures.
    let (mut action, mut original, mut observed): (
        libc::sigaction,
        libc::sigaction,
        libc::sigaction,
    ) = unsafe { std::mem::zeroed() };
    action.sa_sigaction = signal as *const () as usize;
    action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
    // SAFETY: all structures are writable, initialized and live through calls.
    if unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaddset(&mut action.sa_mask, libc::SIGUSR1);
        libc::sigaction(libc::SIGSEGV, &action, &mut original)
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: query only into valid output.
    assert_eq!(
        // SAFETY: query only into valid output.
        unsafe { libc::sigaction(libc::SIGSEGV, ptr::null(), &mut observed) },
        0
    );
    assert_eq!(observed.sa_sigaction, action.sa_sigaction);
    assert_eq!(
        observed.sa_flags & (libc::SA_SIGINFO | libc::SA_ONSTACK),
        action.sa_flags
    );
    // SAFETY: initialized signal set returned by sigaction.
    assert_eq!(
        // SAFETY: initialized signal set returned by sigaction.
        unsafe { libc::sigismember(&observed.sa_mask, libc::SIGUSR1) },
        1
    );
    // SAFETY: explicit owned signal reaches the fixture's registered handler.
    assert_eq!(unsafe { libc::raise(libc::SIGSEGV) }, 0);
    // SAFETY: this deliberately privileged instruction raises #GP in CPL3.
    unsafe {
        core::arch::asm!("in eax, dx",in("dx") 0xffff_u16,out("eax") _,options(nostack));
    }
    assert_eq!(SIGNALS.load(Ordering::Relaxed), 2);
    assert_eq!(HANDLER_ERRORS.load(Ordering::Relaxed), 0);
    // SAFETY: restore exactly the registered original disposition.
    assert_eq!(
        // SAFETY: restore the saved initialized original disposition.
        unsafe { libc::sigaction(libc::SIGSEGV, &original, ptr::null_mut()) },
        0
    );
    Ok(())
}

fn generated_contract(rwx: bool) -> io::Result<()> {
    // SAFETY: fresh writable anonymous page, later executable, finally unmapped.
    let page = unsafe {
        libc::mmap(
            ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE | if rwx { libc::PROT_EXEC } else { 0 },
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if page == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    // PUSH RBX; MOV EAX,7; XOR ECX,ECX; prefixed CPUID; POP RBX; RET.
    let code: [u8; 14] = [
        0x53, 0xb8, 7, 0, 0, 0, 0x31, 0xc9, 0x66, 0x48, 0x0f, 0xa2, 0x5b, 0xc3,
    ];
    // SAFETY: complete code fits fresh mapping, mprotect follows the owned W^X transition.
    unsafe {
        ptr::copy_nonoverlapping(code.as_ptr(), page.cast(), code.len());
    }
    // SAFETY: the mapped page is valid and no writable reference survives.
    if unsafe { libc::mprotect(page, 4096, libc::PROT_READ | libc::PROT_EXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: generated code obeys the SysV no-argument ABI and preserves RBX.
    let call: unsafe extern "C" fn() -> u32 = unsafe { std::mem::transmute(page) };
    // SAFETY: mapping is executable and retained through invocation.
    assert_eq!(unsafe { call() }, 0);
    // SAFETY: function is no longer called after the page is released.
    assert_eq!(unsafe { libc::munmap(page, 4096) }, 0);
    Ok(())
}

fn main() -> io::Result<()> {
    if std::env::var_os("LEGION_CPU_ADAPTER").as_deref() != Some(std::ffi::OsStr::new("1"))
        || !std::path::Path::new("/dev/legion_cpuid").exists()
    {
        return Err(io::Error::other(
            "fixture requires the explicit guest CPUID adapter",
        ));
    }
    if std::env::args().any(|v| v == "--exec-child") {
        assert_eq!(cpuid([7, 0, 0, 0]), native(7, 0)?);
        println!("LEGION_CPU_ADAPTER_EXEC_NATIVE");
        return Ok(());
    }
    if std::env::args().any(|v| v == "--zero-pid") {
        cpuid([0x1337, 0, 0, 0]);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(cpuid([0x1338, 0, 0, 0]), native(0x1338, 0)?);
        let state = std::fs::read("/frames/cpu-adapter-state.bin")?;
        assert_eq!(&state[16..24], &0_u64.to_le_bytes());
        assert_eq!(&state[32..40], &0_u64.to_le_bytes());
        assert_eq!(&state[64..72], &1_u64.to_le_bytes());
        assert_eq!(&state[72..80], &0_u64.to_le_bytes());
        println!("LEGION_KUSER_ZERO_PID_STAYS_PENDING");
        return Ok(());
    }
    signal_contract()?;
    sys_signal_contract()?;
    let initial = cpuid([7, 0, 0, 0]);
    assert_eq!(initial, native(7, 0)?);
    let vendor = match native(0, 0)?[1] {
        0x68747541 => Vendor::Amd,
        0x756e6547 => Vendor::Intel,
        _ => return Err(io::Error::other("unknown vendor")),
    };
    // SAFETY: getpid only observes this Linux process's identity.
    let process = unsafe { libc::getpid() } as u64;
    let payload = 0x51_2345;
    cpuid([0x1337, 0, 0, payload]);
    let mut model = State {
        owner: process,
        declared_process_id: payload,
        handler: 0,
        ready: false,
    };
    let expected = model
        .cpuid(
            vendor,
            Context {
                process,
                privilege: Privilege::User,
                stack_privilege: Privilege::User,
            },
            [7, 0, 0, 0],
            Registers::from_wide(initial),
        )
        .registers;
    // SAFETY: this fixture thread exclusively owns its libc errno slot.
    unsafe {
        *libc::__errno_location() = 123;
    }
    assert_eq!(cpuid([7, 0, 0, 0]), expected);
    // SAFETY: only observes this native thread's libc error slot.
    assert_eq!(unsafe { *libc::__errno_location() }, 123);
    generated_contract(false)?;
    generated_contract(true)?;
    let barrier = Arc::new(Barrier::new(9));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let barrier = Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..256 {
                assert_eq!(cpuid([7, 0, 0, 0]), expected);
                std::thread::yield_now();
            }
        }));
    }
    barrier.wait();
    for worker in workers {
        worker
            .join()
            .map_err(|_| io::Error::other("CPUID worker panicked"))?;
    }
    // SAFETY: workers have joined; the child only observes CPUID and exits.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(io::Error::last_os_error());
    }
    if child == 0 {
        let pass = cpuid([7, 0, 0, 0]) == initial;
        // SAFETY: leave owned fork child without running duplicated destructors.
        unsafe {
            libc::_exit(if pass { 0 } else { 93 });
        }
    }
    let mut status = 0;
    // SAFETY: wait only for the owned child, into live initialized status.
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert_eq!(status, 0);
    assert!(
        std::process::Command::new(std::env::current_exe()?)
            .arg("--exec-child")
            .status()?
            .success()
    );
    assert_eq!(cpuid([7, 0, 0, 0]), expected);
    println!(
        "LEGION_CPU_ADAPTER_FIXTURE_PASSED generated_wx=true generated_rwx=true errno_preserved=true signal_delegation=true workers=8 queries=2048 fork_isolation=true exec_isolation=true"
    );
    Ok(())
}
