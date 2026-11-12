use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;

#[cfg(all(target_arch = "x86_64", not(miri)))]
mod gpu;
#[cfg(all(target_arch = "x86_64", not(miri)))]
mod gpu_kind;

#[cfg(all(target_arch = "x86_64", not(miri)))]
fn mount_filesystems() -> anyhow::Result<()> {
    use anyhow::Context;
    use rustix::mount::{MountFlags, mount};
    for (target, filesystem) in [("/proc", "proc"), ("/sys", "sysfs"), ("/dev", "devtmpfs")] {
        std::fs::create_dir_all(target)?;
        mount(filesystem, target, filesystem, MountFlags::empty(), None)
            .with_context(|| format!("mount guest {filesystem}"))?;
    }
    Ok(())
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
fn cpuid(leaf: u32, subleaf: u32) -> [u32; 4] {
    let r = std::arch::x86_64::__cpuid_count(leaf, subleaf);
    [r.eax, r.ebx, r.ecx, r.edx]
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
fn xgetbv0() -> Option<u64> {
    let features = cpuid(1, 0)[2];
    if features & (1 << 26 | 1 << 27) != (1 << 26 | 1 << 27) {
        return None;
    }
    let low: u32;
    let high: u32;
    // SAFETY: CPUID confirms XSAVE and OSXSAVE; selector zero is defined.
    unsafe {
        std::arch::asm!(
            "xgetbv",
            in("ecx") 0_u32,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    Some(u64::from(low) | u64::from(high) << 32)
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
fn avx512_child() -> ! {
    // SAFETY: this disposable child intentionally tests a CPU exception.
    unsafe {
        std::arch::asm!(
            ".byte 0x62, 0xf1, 0x7d, 0x48, 0xef, 0xc0",
            out("zmm0") _,
            options(nomem, nostack, preserves_flags),
        );
    }
    std::process::exit(0)
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
fn cpuid_fault_child() -> ! {
    // SAFETY: ARCH_SET_CPUID changes only this disposable guest child's permission to execute CPUID.
    let result = unsafe { libc::syscall(libc::SYS_arch_prctl, 0x1012_i64, 0_i64) };
    if result != 0 {
        eprintln!("ARCH_SET_CPUID: {}", std::io::Error::last_os_error());
        std::process::exit(77);
    }
    // Execution should produce SIGSEGV when task-local CPUID faulting works.
    std::hint::black_box(cpuid(0, 0));
    std::process::exit(0)
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--cpuid-fault-child")
        && std::env::var_os("LEGION_GUEST_CHILD").is_some()
        && std::fs::read_link("/proc/1/exe").is_ok_and(|path| path == std::path::Path::new("/init"))
    {
        cpuid_fault_child();
    }
    if std::env::args().nth(1).as_deref() == Some("--read-xcr0") {
        match xgetbv0() {
            Some(value) => println!("XCR0={value:#x}"),
            None => eprintln!("XSAVE/OSXSAVE is unavailable"),
        }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--avx512-child")
        && std::env::var_os("LEGION_GUEST_CHILD").is_some()
    {
        avx512_child();
    }
    if std::process::id() != 1 {
        eprintln!("legion-guest-probe must run as /init in the disposable guest");
        std::process::exit(1);
    }

    let leaf0 = cpuid(0, 0);
    let leaf1 = cpuid(1, 0);
    let leaf7 = cpuid(7, 0);
    let leafd = cpuid(13, 0);
    let xcr0 = xgetbv0();
    let topology = mount_filesystems().and_then(|()| legion_cpu_topology::Topology::detect());
    let (topology, topology_error) = match topology {
        Ok(topology) => (Some(topology), None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let sigill = Command::new("/init")
        .arg("--avx512-child")
        .env("LEGION_GUEST_CHILD", "1")
        .status()
        .ok()
        .and_then(|status| status.signal())
        == Some(4);
    let passed = xcr0 == Some(7)
        && leaf7[1] & (1 << 16) == 0
        && leafd[0] == 7
        && sigill
        && topology.is_some();
    let cpuid_fault = Command::new("/init")
        .arg("--cpuid-fault-child")
        .env("LEGION_GUEST_CHILD", "1")
        .output();
    let capability = match cpuid_fault {
        Ok(output) => serde_json::json!({"schema":1,"supported":output.status.signal()==Some(11),
            "exit_code":output.status.code(),"signal":output.status.signal(),
            "stderr":String::from_utf8_lossy(&output.stderr),"scope":"owned guest child only"}),
        Err(error) => serde_json::json!({"schema":1,"supported":false,"error":error.to_string()}),
    };
    println!("LEGION_CPUID_FAULT_PROBE {capability}");
    // One machine-readable line.
    println!(
        "LEGION_PROBE {}",
        serde_json::json!({"schema":2,"passed":passed,"xcr0":xcr0,"avx512_sigill":sigill,"leaf0":leaf0,"leaf1":leaf1,"leaf7":leaf7,"leafd0":leafd,"topology":topology,"topology_error":topology_error})
    );
    let _ = std::io::stdout().flush();
    if std::path::Path::new("/modules/load-order").is_file() {
        if std::fs::read_to_string("/proc/cmdline").is_ok_and(|cmdline| {
            cmdline
                .split_whitespace()
                .any(|arg| arg == "legion.drm_probe=1")
        }) {
            let report = gpu::drm_report();
            println!("LEGION_DRM_PROBE {report}");
            let _ = std::io::stdout().flush();
            if report["passed"] == true
                && std::fs::read_to_string("/proc/cmdline")
                    .is_ok_and(|text| text.split_whitespace().any(|arg| arg == "legion.vulkan=1"))
            {
                println!("LEGION_VULKAN_PROBE {}", gpu::vulkan_report());
                let _ = std::io::stdout().flush();
            }
            loop {
                std::thread::park();
            }
        }
        let report = gpu::report();
        println!("LEGION_GPU_PROBE {report}");
        let _ = std::io::stdout().flush();
        if std::fs::read_to_string("/proc/cmdline").is_ok_and(|cmdline| {
            cmdline
                .split_whitespace()
                .any(|arg| arg == "legion.vulkan=1")
        }) {
            println!("LEGION_VULKAN_PROBE {}", gpu::vulkan_report());
            let _ = std::io::stdout().flush();
        }
    }
    // PID1 must stay alive until the host captures the report.
    loop {
        std::thread::park();
    }
}

#[cfg(any(not(target_arch = "x86_64"), miri))]
fn main() {
    eprintln!("the hardware fixture requires a real x86-64 KVM guest");
}
