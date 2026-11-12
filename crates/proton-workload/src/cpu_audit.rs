//! Owned Windows-process observation.
use super::{Handle, Result, log, open, write_handle};
use alloc::{collections::BTreeMap, format, string::ToString, vec::Vec};
use legion_cpu_contract::{POLL_READY, REGISTER_PROCESS, Registers, Vendor, kuser, profile_reply};
use serde_json::json;
use windows::{
    Win32::System::{
        Environment::GetCommandLineW,
        LibraryLoader::{GetModuleFileNameW, GetModuleHandleW, GetProcAddress},
        Threading::{
            CreateProcessW, GetExitCodeProcess, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
            STARTUPINFOW,
        },
    },
    core::{PCWSTR, PWSTR, s, w},
};

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn GetCurrentProcessId() -> u32;
    fn GetEnvironmentVariableW(name: *const u16, output: *mut u16, count: u32) -> u32;
    fn ReadProcessMemory(
        process: *mut core::ffi::c_void,
        address: *const core::ffi::c_void,
        buffer: *mut core::ffi::c_void,
        size: usize,
        read: *mut usize,
    ) -> i32;
    fn Sleep(milliseconds: u32);
}

fn enabled(name: *const u16) -> bool {
    let mut flag = [0_u16; 2];
    // SAFETY: caller provides a constant NUL-terminated name and bounded output.
    unsafe {
        GetEnvironmentVariableW(name, flag.as_mut_ptr(), 2) == 1 && flag[0] == u16::from(b'1')
    }
}

fn page() -> Result<[u8; kuser::PAGE_SIZE]> {
    let mut bytes = [0_u8; kuser::PAGE_SIZE];
    let mut read = 0;
    // SAFETY: pseudo current-process handle; the OS copies the known KUSER range into our owned buffer without a Rust reference to the shared page.
    if unsafe {
        ReadProcessMemory(
            -1_isize as *mut _,
            0x7ffe_0000 as *const _,
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            &mut read,
        )
    } == 0
        || read != bytes.len()
    {
        return Err("KUSER capture failed".into());
    }
    Ok(bytes)
}

pub(super) fn observe_if_requested() -> Result<bool> {
    // SAFETY: the OS owns this complete NUL-terminated command line for the process lifetime.
    let live = unsafe { GetCommandLineW().to_string() }
        .map_err(|e| e.to_string())?
        .ends_with(" --legion-kuser-observer");
    let mut flag = [0_u16; 2];
    // SAFETY: constant NUL-terminated name and bounded writable output.
    let length = unsafe {
        GetEnvironmentVariableW(w!("LEGION_KUSER_OBSERVER").as_ptr(), flag.as_mut_ptr(), 2)
    };
    if length == 0 && !live {
        return Ok(false);
    }
    let label = if live {
        "live"
    } else {
        match (length, flag[0]) {
            (1, 49) => "before",
            (1, 50) => "after",
            _ => return Err("invalid KUSER observer mode".into()),
        }
    };
    let bytes = page()?;
    let output = open(&format!("Z:\\frames\\kuser-observer-{label}.bin"), true)?;
    write_handle(output.0, &bytes)?;
    let output = open(&format!("Z:\\frames\\kuser-observer-{label}.json"), true)?;
    write_handle(
        output.0,
        &serde_json::to_vec(&json!({"leaf1":cpuid([1,0,0,0]),"xcr0":xcr0()}))
            .map_err(|e| e.to_string())?,
    )?;
    log("LEGION_KUSER_UNREGISTERED_OBSERVER_CAPTURED");
    Ok(true)
}

fn observe_live_child() -> Result<()> {
    let mut executable = [0_u16; 4096];
    // SAFETY: owned output, null module selects this fixture executable.
    let count = unsafe { GetModuleFileNameW(None, &mut executable) } as usize;
    if count == 0 || count >= executable.len() {
        return Err("get observer executable path failed".into());
    }
    let mut command = Vec::from([u16::from(b'"')]);
    command.extend_from_slice(&executable[..count]);
    command.extend("\" --legion-kuser-observer\0".encode_utf16());
    let startup = STARTUPINFOW {
        cb: core::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: explicit owned executable, writable NUL-terminated command, initialized C outputs, inherited environment, no inherited handles.
    unsafe {
        CreateProcessW(
            PCWSTR(executable.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            PROCESS_CREATION_FLAGS(0),
            None,
            PCWSTR::null(),
            &startup,
            &mut process,
        )
    }
    .map_err(|e| e.to_string())?;
    let owned_process = Handle(process.hProcess.0);
    let _owned_thread = Handle(process.hThread.0);
    // SAFETY: waits only for the child just created; both handles remain live.
    if unsafe { super::WaitForSingleObject(owned_process.0, 10_000) } != 0 {
        return Err("live KUSER observer child timed out".into());
    }
    let mut exit = 0;
    // SAFETY: live owned process handle and initialized writable exit output.
    unsafe { GetExitCodeProcess(process.hProcess, &mut exit) }.map_err(|e| e.to_string())?;
    if exit != 0 {
        return Err(format!("live KUSER observer failed: {exit}"));
    }
    Ok(())
}

fn cpuid(mut registers: [u64; 4]) -> [u64; 4] {
    // SAFETY: CPUID exists on every x86-64 CPU.
    unsafe {
        core::arch::asm!(
            "xchg {saved}, rbx", "cpuid", "xchg {saved}, rbx",
            saved = inout(reg) registers[1], inout("rax") registers[0],
            inout("rcx") registers[2], inout("rdx") registers[3],
            options(nostack, preserves_flags),
        );
    }
    registers
}

fn xcr0() -> Option<u64> {
    let features = cpuid([1, 0, 0, 0])[2];
    if features & (1 << 26 | 1 << 27) != (1 << 26 | 1 << 27) {
        return None;
    }
    let low: u32;
    let high: u32;
    // SAFETY: XSAVE and OSXSAVE were checked above.
    unsafe {
        core::arch::asm!("xgetbv", in("ecx") 0_u32, out("eax") low, out("edx") high,
        options(nomem, nostack, preserves_flags));
    }
    Some(u64::from(low) | u64::from(high) << 32)
}

fn wine_enabled_xstate() -> Option<u64> {
    // SAFETY: constant module name is NUL terminated.
    let kernel = unsafe { GetModuleHandleW(w!("kernel32.dll")) }.ok()?;
    // SAFETY: lookup uses the documented exported function's exact name.
    let function = unsafe { GetProcAddress(kernel, s!("GetEnabledXStateFeatures")) }?;
    // SAFETY: the selected Windows API has this system calling convention, no arguments and a u64 result.
    let enabled: unsafe extern "system" fn() -> u64 = unsafe { core::mem::transmute(function) };
    // SAFETY: invoking the verified public API while its owning DLL is loaded.
    Some(unsafe { enabled() })
}

pub(super) fn run_if_requested() -> Result<()> {
    let mut flag = [0_u16; 2];
    // SAFETY: fixed NUL-terminated name and writable two-element output.
    let length = unsafe {
        GetEnvironmentVariableW(
            w!("LEGION_CPU_PROTOCOL_AUDIT").as_ptr(),
            flag.as_mut_ptr(),
            2,
        )
    };
    if length == 0 {
        return Ok(());
    }
    if length != 1 || flag[0] != u16::from(b'1') {
        return Err("invalid CPU audit opt-in".into());
    }
    let before0 = cpuid([0, 0, 0, 0]);
    let vendor = match (before0[1] as u32, before0[3] as u32, before0[2] as u32) {
        (0x6874_7541, 0x6974_6e65, 0x444d_4163) => Vendor::Amd,
        (0x756e_6547, 0x4965_6e69, 0x6c65_746e) => Vendor::Intel,
        _ => return Err("unrecognized CPU vendor in audit".into()),
    };
    let before1 = cpuid([1, 0, 0, 0]);
    let before7 = cpuid([7, 0, 0, 0]);
    let befored = cpuid([13, 0, 0, 0]);
    let kuser_requested = enabled(w!("LEGION_KUSER_WORKER").as_ptr());
    let kuser_before = if kuser_requested { Some(page()?) } else { None };
    // SAFETY: GetCurrentProcessId has no inputs and only observes this process.
    let pid = unsafe { GetCurrentProcessId() };
    let pending_input = [
        u64::from(POLL_READY),
        0x1234_5678_9abc_def0,
        0xfedc_ba98_7654_3210,
        0x1111_2222_3333_4444,
    ];
    let before_ready = cpuid(pending_input);
    let registration = cpuid([u64::from(REGISTER_PROCESS), 0, 0, u64::from(pid)]);
    let mut ready_first = cpuid(pending_input);
    if kuser_requested {
        for _ in 0..2000 {
            if ready_first[0] == 1 {
                break;
            }
            // SAFETY: yields this owned Windows thread for the asynchronous worker.
            unsafe {
                Sleep(1);
            }
            ready_first = cpuid(pending_input);
        }
        if ready_first != [1, pending_input[1], pending_input[2], pending_input[3]] {
            return Err("KUSER worker did not publish valid readiness".into());
        }
    }
    let ready_second = cpuid(pending_input);
    let kuser_report = if let Some(before) = kuser_before {
        let after = page()?;
        if !kuser::matches(vendor, &after) {
            return Err("KUSER source recipe mismatch".into());
        }
        if before[0x308..0x30c] != after[0x308..0x30c]
            || before[0x3d8..0x818] != after[0x3d8..0x818]
        {
            return Err("KUSER changed Wine dispatch/XSTATE metadata".into());
        }
        // SAFETY: bounded wait permits native source and private clock updates.
        unsafe {
            Sleep(50);
        }
        let later = page()?;
        let time = |page: &[u8; kuser::PAGE_SIZE], offset| -> u64 {
            u64::from_le_bytes(
                page[offset..offset + 8]
                    .try_into()
                    .expect("fixed page time field"),
            )
        };
        let clock_advances =
            time(&later, 8) > time(&after, 8) && time(&later, 0x320) > time(&after, 0x320);
        if !clock_advances {
            return Err("private KUSER clock stopped".into());
        }
        observe_live_child()?;
        for (name, bytes) in [("before", before), ("after", after), ("later", later)] {
            let output = open(&format!("Z:\\frames\\kuser-{name}.bin"), true)?;
            write_handle(output.0, &bytes)?;
        }
        Some(
            json!({"source_recipe_matches":true,"native_dispatch_preserved":true,
            "native_xstate_preserved":true,"clocks_advance":true,"private_page_published":true}),
        )
    } else {
        None
    };
    let after1 = cpuid([1, 0, 0, 0]);
    let mut samples = Vec::new();
    let mut differences = Vec::new();
    let mut mismatched_leaves = BTreeMap::<u32, u32>::new();
    let mut inherited = 0_u32;
    for leaf in (0..=0x40).chain(0x8000_0000..=0x8000_0030).chain([
        0x4000_0000,
        0x4000_0001,
        0x8000_0100,
        0xffff_ffff,
    ]) {
        for subleaf in 0..33 {
            let observed = cpuid([u64::from(leaf), 0, u64::from(subleaf), 0]);
            // Source-inherited fields use the observed low words.
            let expected = profile_reply(vendor, leaf, subleaf, Registers::from_wide(observed));
            if let Some(expected) = expected {
                if expected != observed {
                    *mismatched_leaves.entry(leaf).or_default() += 1;
                    differences.push(json!({"leaf":leaf,"subleaf":subleaf,"expected":expected,"actual":observed}));
                }
            } else {
                inherited += 1;
            }
            samples.push(json!({"leaf":leaf,"subleaf":subleaf,"actual":observed}));
        }
    }
    let report = json!({
        "schema":1,"capture_complete":true,"scope":"owned Windows process inside selected Proton",
        "profile":match vendor { Vendor::Amd=>"amd",Vendor::Intel=>"intel" },"windows_pid":pid,
        "xcr0":xcr0(),"wine_enabled_xstate":wine_enabled_xstate(),
        "before_registration":{"leaf0":before0,"leaf1":before1,"leaf7":before7,"leafd0":befored},
        "registration_reply":registration,"after_registration_leaf1":after1,
        "ordinary_fixed_and_masked_profile_matches":differences.is_empty(),
        "queries":samples.len(),"mismatches":differences.len(),"mismatched_leaves":mismatched_leaves,
        "inherited_native_queries":inherited,"independent_native_passthrough_verified":false,
        "readiness":{"input":pending_input,"before_registration":before_ready,"first":ready_first,"second":ready_second,
            "owned_one_shot_observed":ready_first==[1,pending_input[1],pending_input[2],pending_input[3]] && ready_first!=ready_second},
        "handler_registration_attempted":false,"handler_execution_verified":false,
        "kuser_memory_writes_verified":kuser_requested,"kuser":kuser_report,"complete_profile_verified":false,
        "first_mismatches":differences.iter().take(16).collect::<Vec<_>>(),"samples":samples,
    });
    let bytes = serde_json::to_vec(&report).map_err(|error| error.to_string())?;
    let output = open("Z:\\frames\\cpu-protocol-audit.json", true)?;
    write_handle(output.0, &bytes)?;
    log(&format!(
        "LEGION_CPU_PROTOCOL_AUDIT_CAPTURED queries={} mismatches={}",
        report["queries"], report["mismatches"]
    ));
    Ok(())
}
