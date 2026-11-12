use super::{Result, log, open, write_handle};
use alloc::{format, string::ToString, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use legion_cpu_contract::{BYPASS_MAGIC, REGISTER_HANDLER};
use serde::Serialize;
#[used]
#[unsafe(no_mangle)]
static LEGION_NT_ACTIVE_CASE: AtomicU64 = AtomicU64::new(0);
#[used]
#[unsafe(no_mangle)]
static LEGION_NT_UNEXPECTED: AtomicU64 = AtomicU64::new(0);
#[used]
#[unsafe(no_mangle)]
static LEGION_NT_FORWARD_OTHER: AtomicU64 = AtomicU64::new(0);
#[used]
#[unsafe(no_mangle)]
static LEGION_NT_OTHER_COUNT: AtomicU64 = AtomicU64::new(0);
#[used]
#[unsafe(no_mangle)]
static LEGION_NT_BYPASS: u64 = BYPASS_MAGIC;
static ENABLED: AtomicBool = AtomicBool::new(false);
use windows::{
    Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress},
    core::{s, w},
};

#[repr(C)]
#[derive(Default, Serialize)]
struct Case {
    time: u64,
    service: u64,
    marker: [u64; 2],
    seeded_xmm4: [u64; 2],
    expected_return: u64,
    stack_before: u64,
    result: u64,
    after_xmm4: [u64; 2],
    after_xmm5: [u64; 2],
    seen_rax: u64,
    seen_rcx: u64,
    seen_r11: u64,
    seen_rsp: u64,
    seen_xmm4: [u64; 2],
    seen_xmm5: [u64; 2],
    delivered: u64,
    flags_before: u64,
    stub_entry: u64,
    want_xmm6: [u64; 2],
    got_xmm6: [u64; 2],
    want_xmm15: [u64; 2],
    got_xmm15: [u64; 2],
    want_rdi: u64,
    want_rsi: u64,
    got_rdi: u64,
    got_rsi: u64,
}
const _: () = assert!(core::mem::size_of::<Case>() == 288);
core::arch::global_asm!(include_str!("nt_audit.S"));
unsafe extern "system" {
    fn legion_nt_raw_case(case: *mut Case);
    fn legion_nt_capture();
    fn legion_nt_wrong();
    fn legion_nt_raw_end();
    fn legion_nt_stub_case(case: *mut Case);
}
#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn GetEnvironmentVariableW(name: *const u16, output: *mut u16, count: u32) -> u32;
    fn VirtualAlloc(
        address: *mut core::ffi::c_void,
        size: usize,
        kind: u32,
        protection: u32,
    ) -> *mut core::ffi::c_void;
    fn VirtualProtect(
        address: *mut core::ffi::c_void,
        size: usize,
        protection: u32,
        old: *mut u32,
    ) -> i32;
    fn VirtualFree(address: *mut core::ffi::c_void, size: usize, kind: u32) -> i32;
}

fn cpuid(mut registers: [u64; 4]) {
    // SAFETY: CPUID protocol requests are confined to this opted-in guest process; reserved RBX is exchanged and restored around the instruction.
    unsafe {
        core::arch::asm!("xchg {saved}, rbx","cpuid","xchg {saved}, rbx",
        saved=inout(reg) registers[1],inout("rax") registers[0],inout("rcx") registers[2],
        inout("rdx") registers[3],options(nostack,preserves_flags));
    }
    core::hint::black_box(registers);
}

fn service_id() -> Result<u32> {
    // SAFETY: this process imports ntdll and keeps it loaded through the test.
    let module = unsafe { GetModuleHandleW(w!("ntdll.dll")) }.map_err(|e| e.to_string())?;
    // SAFETY: exact exported public NT function name in the loaded module.
    let address = unsafe { GetProcAddress(module, s!("NtQuerySystemTime")) }
        .ok_or("missing NtQuerySystemTime")?;
    // SAFETY: the exported stub starts in loaded module text.
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, 8) };
    if bytes[..4] != [0x4c, 0x8b, 0xd1, 0xb8] {
        return Err("unsupported NT stub prologue".into());
    }
    Ok(u32::from_le_bytes(
        bytes[4..8]
            .try_into()
            .map_err(|_| "invalid service field")?,
    ))
}

fn stub_info() -> Result<(u64, u64)> {
    // SAFETY: loaded ntdll export is retained by this process's imports.
    let function = unsafe {
        let module = GetModuleHandleW(w!("ntdll.dll")).map_err(|e| e.to_string())?;
        GetProcAddress(module, s!("NtQuerySystemTime"))
    }
    .ok_or("missing NT test stub")?;
    let address = function as *const u8;
    // SAFETY: the selected export's instruction stub occupies readable loaded module text.
    let bytes = unsafe { core::slice::from_raw_parts(address, 32) };
    let pattern = [0xff, 0x14, 0x25, 0, 0x10, 0xfe, 0x7f];
    let offset = bytes
        .windows(pattern.len())
        .position(|v| v == pattern)
        .ok_or("unsupported owned NT dispatcher CALL encoding")?;
    let syscall = bytes[..offset]
        .windows(3)
        .position(|v| v == [0x0f, 0x05, 0xc3])
        .ok_or("missing owned NT SYSCALL/RET branch")?;
    Ok((address as u64, address as u64 + (syscall + 2) as u64))
}

fn ordinary_stub_probe() -> Result<()> {
    // SAFETY: loaded ntdll remains live and the named export has the public NTSTATUS NtQuerySystemTime(LARGE_INTEGER *) Windows ABI.
    let function = unsafe {
        let module = GetModuleHandleW(w!("ntdll.dll")).map_err(|e| e.to_string())?;
        GetProcAddress(module, s!("NtQuerySystemTime"))
    }
    .ok_or("missing ordinary NT export")?;
    // SAFETY: exact documented export signature verified by its known name.
    let call = unsafe {
        core::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            unsafe extern "system" fn(*mut i64) -> i32,
        >(function)
    };
    let before = LEGION_NT_OTHER_COUNT.load(Ordering::Acquire);
    let mut time = 0_i64;
    // SAFETY: valid writable LARGE_INTEGER storage through the synchronous call.
    let status = unsafe { call(&mut time) };
    let entered = LEGION_NT_OTHER_COUNT.load(Ordering::Acquire) != before;
    let output = open("Z:\\frames\\ordinary-nt-stub.json", true)?;
    write_handle(
        output.0,
        &serde_json::to_vec(&serde_json::json!({"schema":1,"status":status,
        "time_written":time>0,"entered_callback":entered}))
        .map_err(|e| e.to_string())?,
    )?;
    if status != 0 || time <= 0 || entered != (LEGION_NT_FORWARD_OTHER.load(Ordering::Acquire) != 0)
    {
        return Err("ordinary Wine stub behavior differs from direct-only scope".into());
    }
    Ok(())
}

fn null_argument_probe() -> Result<()> {
    // SAFETY: NtClose accepts a HANDLE; null is an invalid handle and produces the documented NT error without closing a resource owned by anyone.
    let function = unsafe {
        let module = GetModuleHandleW(w!("ntdll.dll")).map_err(|e| e.to_string())?;
        GetProcAddress(module, s!("NtClose"))
    }
    .ok_or("missing NtClose")?;
    // SAFETY: exact NTSTATUS NtClose(HANDLE) calling convention.
    let call = unsafe {
        core::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            unsafe extern "system" fn(*mut core::ffi::c_void) -> i32,
        >(function)
    };
    let before = LEGION_NT_OTHER_COUNT.load(Ordering::Acquire);
    // SAFETY: intentional invalid-handle request, with no resource side effect.
    let status = unsafe { call(core::ptr::null_mut()) } as u32;
    let entered = LEGION_NT_OTHER_COUNT.load(Ordering::Acquire) != before;
    let output = open("Z:\\frames\\ordinary-nt-null.json", true)?;
    write_handle(
        output.0,
        &serde_json::to_vec(
            &serde_json::json!({"schema":1,"status":status,"entered_callback":entered}),
        )
        .map_err(|e| e.to_string())?,
    )?;
    if status != 0xc0000008 || entered != (LEGION_NT_FORWARD_OTHER.load(Ordering::Acquire) != 0) {
        return Err("ordinary null-handle forwarding failed".into());
    }
    Ok(())
}

fn execute(case: &mut Case) {
    execute_with(case, legion_nt_raw_case);
}

fn execute_with(case: &mut Case, call: unsafe extern "system" fn(*mut Case)) {
    // Only this exact live record is writable by the owned callback.
    LEGION_NT_ACTIVE_CASE.store(core::ptr::from_mut(case) as u64, Ordering::Release);
    // SAFETY: the wrapper preserves Win64 nonvolatile state.
    unsafe {
        call(case);
    }
    LEGION_NT_ACTIVE_CASE.store(0, Ordering::Release);
}

struct Generated(*mut core::ffi::c_void);
impl Generated {
    fn new() -> Result<Self> {
        let begin = legion_nt_raw_case as *const u8;
        let size = (legion_nt_raw_end as *const () as usize)
            .checked_sub(begin as usize)
            .ok_or("invalid owned wrapper range")?;
        if !(1..=1024).contains(&size) {
            return Err("invalid owned wrapper length".into());
        }
        // SAFETY: allocate only this owned 4 KiB fixture page, initially RW.
        let memory = unsafe { VirtualAlloc(core::ptr::null_mut(), 4096, 0x3000, 0x04) };
        if memory.is_null() {
            return Err("generated NT allocation failed".into());
        }
        let owned = Self(memory);
        // SAFETY: the validated wrapper text is readable, fits this fresh page, and uses only internal RIP-relative labels and the supplied Case.
        unsafe {
            core::ptr::copy_nonoverlapping(begin, memory.cast(), size);
        }
        let mut old = 0;
        // SAFETY: publish only this owned allocation as RX after copying.
        if unsafe { VirtualProtect(memory, 4096, 0x20, &mut old) } == 0 {
            return Err("generated NT protection failed".into());
        }
        Ok(owned)
    }
    fn execute(&self, case: &mut Case) {
        // SAFETY: this live RX page contains the exact owned wrapper with the Win64 one-pointer ABI and no external relative branches.
        let call = unsafe {
            core::mem::transmute::<*mut core::ffi::c_void, unsafe extern "system" fn(*mut Case)>(
                self.0,
            )
        };
        execute_with(case, call);
    }
}
impl Drop for Generated {
    fn drop(&mut self) {
        // SAFETY: this fixture uniquely owns the completed generated allocation.
        unsafe {
            VirtualFree(self.0, 0, 0x8000);
        }
    }
}

pub(super) fn finish() -> Result<()> {
    if !ENABLED.load(Ordering::Acquire) {
        return Ok(());
    }
    let unexpected = LEGION_NT_UNEXPECTED.load(Ordering::Acquire);
    let output = open("Z:\\frames\\nt-syscall-completion.json", true)?;
    write_handle(
        output.0,
        &serde_json::to_vec(
            &serde_json::json!({"schema":1,"unexpected_callbacks":unexpected,"ordinary_callbacks_forwarded":LEGION_NT_OTHER_COUNT.load(Ordering::Acquire),
        "passed":unexpected==0}),
        )
        .map_err(|e| e.to_string())?,
    )?;
    if unexpected != 0 {
        return Err("unexpected NT callback outside owned cases".into());
    }
    Ok(())
}

pub(super) fn run_if_requested() -> Result<()> {
    let mut flag = [0_u16; 2];
    // SAFETY: fixed NUL-terminated name, bounded writable buffer.
    if unsafe { GetEnvironmentVariableW(w!("LEGION_NT_SYSCALL").as_ptr(), flag.as_mut_ptr(), 2) }
        == 0
    {
        return Ok(());
    }
    if flag[0] != u16::from(b'1') {
        return Err("invalid NT syscall audit opt-in".into());
    }
    ENABLED.store(true, Ordering::Release);
    let mut dispatch = [0_u16; 2];
    // SAFETY: fixed fixture-only flag name and bounded writable output.
    if unsafe {
        GetEnvironmentVariableW(
            w!("LEGION_WINE_DISPATCH").as_ptr(),
            dispatch.as_mut_ptr(),
            2,
        )
    } == 1
        && dispatch[0] == 49
    {
        LEGION_NT_FORWARD_OTHER.store(1, Ordering::Release);
    }
    let service = service_id()?;
    let make = |marker: [u64; 2]| Case {
        service: u64::from(service) | 0x1234_5678_0000_0000,
        marker,
        seeded_xmm4: [0x1111_2222_3333_4444, 0x5555_6666_7777_8888],
        ..Default::default()
    };
    let mut cases = Vec::new();
    let mut invalid = make([0, 0]);
    invalid.service = u64::MAX;
    execute(&mut invalid);
    if ![0xc000_000d, 0xc000_001c].contains(&invalid.result) {
        return Err(format!(
            "Wine direct-syscall dispatch unavailable: {:#x}",
            invalid.result
        ));
    }
    cases.push(invalid);
    let mut native = make([0x1020_3040_5060_7080, 0xfeed_face_dead_beef]);
    // SAFETY: this owned wrapper preserves Win64 nonvolatile state and submits only NtQuerySystemTime, with a valid writable first argument.
    execute(&mut native);
    if native.result != 0 || native.time == 0 || native.delivered != 0 {
        return Err("unregistered direct NT forwarding failed".into());
    }
    cases.push(native);
    let dispatched = LEGION_NT_FORWARD_OTHER.load(Ordering::Acquire) != 0;
    let (stub, stub_return) = if dispatched { stub_info()? } else { (0, 0) };
    let make_stub = |marker| Case {
        service: u64::from(service),
        stub_entry: stub,
        expected_return: stub_return,
        want_xmm6: [0x1234_5678_abcd_ef01, 0x9abc_def0_1234_5678],
        want_xmm15: [0xfedc_ba98_7654_3210, 0x1122_3344_5566_7788],
        want_rdi: 0x1234_aaaa_5555_5678,
        want_rsi: 0xfedc_bbbb_6666_9876,
        ..make(marker)
    };
    let mut stub_cases = Vec::new();
    if dispatched {
        let mut ordinary = make_stub([0x1020_3040_5060_7080, 0xfeed_face_dead_beef]);
        execute_with(&mut ordinary, legion_nt_stub_case);
        if ordinary.result != 0 || ordinary.time == 0 || ordinary.delivered != 0 {
            return Err("ordinary NT call before handler failed".into());
        }
        stub_cases.push(ordinary);
    }
    cpuid([u64::from(REGISTER_HANDLER), 0, 0, 0]);
    cpuid([
        u64::from(REGISTER_HANDLER),
        0,
        legion_nt_capture as *const () as u64,
        0,
    ]);

    cpuid([
        u64::from(REGISTER_HANDLER),
        0,
        legion_nt_wrong as *const () as u64,
        0,
    ]);
    for marker in [
        [0x1020_3040_5060_7080, 0xfeed_face_dead_beef],
        [0, 0],
        [BYPASS_MAGIC ^ 1, u64::MAX],
    ] {
        let mut redirected = make(marker);
        // SAFETY: same owned wrapper; the callback records into this live Case.
        execute(&mut redirected);
        if redirected.delivered != 1
            || redirected.result != 0x5a5a_5a5a
            || redirected.seen_rax != redirected.expected_return
            || redirected.seen_rcx != legion_nt_capture as *const () as u64
            || redirected.seen_rsp != redirected.stack_before
            || redirected.seen_xmm4 != [u64::from(service), 0]
            || redirected.seen_xmm5 != marker
            || redirected.seen_r11 != redirected.flags_before
        {
            return Err(format!(
                "direct NT handler ABI mismatch: {}",
                serde_json::to_string(&redirected).map_err(|e| e.to_string())?
            ));
        }
        cases.push(redirected);
    }
    let mut bypass = make([BYPASS_MAGIC, 0xfeed_face_dead_beef]);
    // SAFETY: the marker routes the same valid NtQuerySystemTime call to Wine.
    execute(&mut bypass);
    if bypass.result != 0 || bypass.time == 0 || bypass.delivered != 0 {
        return Err("marked direct NT forwarding failed".into());
    }
    cases.push(bypass);
    if dispatched {
        for marker in [
            [0x1020_3040_5060_7080, 0xfeed_face_dead_beef],
            [0, 0],
            [BYPASS_MAGIC ^ 1, u64::MAX],
            [BYPASS_MAGIC, 0xfeed_face_dead_beef],
        ] {
            let mut ordinary = make_stub(marker);
            execute_with(&mut ordinary, legion_nt_stub_case);
            let bypass = marker[0] == BYPASS_MAGIC;
            if ordinary.delivered != u64::from(!bypass)
                || ordinary.result != if bypass { 0 } else { 0x5a5a_5a5a }
                || ordinary.got_xmm6 != ordinary.want_xmm6
                || ordinary.got_xmm15 != ordinary.want_xmm15
                || ordinary.got_rdi != ordinary.want_rdi
                || ordinary.got_rsi != ordinary.want_rsi
            {
                return Err(format!(
                    "Wine dispatcher ABI mismatch: {}",
                    serde_json::to_string(&ordinary).map_err(|e| e.to_string())?
                ));
            }
            if !bypass
                && (ordinary.seen_rax != ordinary.expected_return
                    || ordinary.seen_rcx != legion_nt_capture as *const () as u64
                    || ordinary.seen_rsp != ordinary.stack_before
                    || ordinary.seen_r11 != ordinary.flags_before
                    || ordinary.seen_xmm4 != [u64::from(service), 0]
                    || ordinary.seen_xmm5 != marker)
            {
                return Err("ordinary NT source registers mismatch".into());
            }
            stub_cases.push(ordinary);
        }
    }
    let generated = Generated::new()?;
    let mut generated_redirect = make([0, 0]);
    generated.execute(&mut generated_redirect);
    if generated_redirect.delivered != 1
        || generated_redirect.result != 0x5a5a_5a5a
        || generated_redirect.seen_rax != generated_redirect.expected_return
        || generated_redirect.seen_rsp != generated_redirect.stack_before
        || generated_redirect.seen_xmm4 != [u64::from(service), 0]
    {
        return Err("generated direct NT delivery failed".into());
    }
    cases.push(generated_redirect);
    let mut generated_bypass = make([BYPASS_MAGIC, 0xfeed_face_dead_beef]);
    generated.execute(&mut generated_bypass);
    if generated_bypass.delivered != 0 || generated_bypass.result != 0 || generated_bypass.time == 0
    {
        return Err("generated direct NT forwarding failed".into());
    }
    cases.push(generated_bypass);
    ordinary_stub_probe()?;
    null_argument_probe()?;
    let output = open("Z:\\frames\\nt-syscall-audit.json", true)?;
    let report = serde_json::json!({"schema":1,"passed":true,"service":service,"scope":"owned direct SYSCALL and optional Wine dispatcher",
        "wine_stub_physical_return_verified":dispatched,
        "handler_first_nonzero_verified":true,"handler_delivery_verified":true,"wine_forwarding_verified":true,
        "generated_code_verified":true,"normal_wine_dispatcher_intercepted":LEGION_NT_FORWARD_OTHER.load(Ordering::Acquire)!=0,"complete_profile_verified":false,
        "case_names":["invalid_before_handler","native_before_handler","redirect_opaque","redirect_zero","redirect_near_magic","bypass_wide_marker","generated_redirect","generated_bypass"],
        "stub_cases":stub_cases,"cases":cases});
    write_handle(
        output.0,
        &serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
    )?;
    log("LEGION_NT_SYSCALL_AUDIT_PASSED");
    Ok(())
}
