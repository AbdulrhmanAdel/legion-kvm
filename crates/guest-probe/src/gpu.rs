//! Runs only in the disposable initramfs, after the PID1 check in main.
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};

use super::gpu_kind::GpuKind;
use anyhow::{Context, Result, ensure};
use rustix::mount::{MountFlags, mount};
use serde_json::{Value, json};

fn selected_gpu() -> Result<GpuKind> {
    GpuKind::from_cmdline(&fs::read_to_string("/proc/cmdline")?)
}

pub fn vulkan_report() -> Value {
    match inspect_vulkan() {
        Ok(report) => report,
        Err(error) => json!({"schema": 1, "passed": false, "error": format!("{error:#}")}),
    }
}

fn record_stage(stage: &str) -> Result<()> {
    use std::io::Write;
    let status = json!({"schema":1,"stage":stage});
    println!("LEGION_GUEST_STAGE {status}");
    std::io::stdout().flush()?;
    if Path::new("/frames").is_dir() {
        fs::write("/frames/guest-stage.json.tmp", serde_json::to_vec(&status)?)?;
        fs::rename("/frames/guest-stage.json.tmp", "/frames/guest-stage.json")?;
    }
    Ok(())
}

/// Generic virtio-GPU plumbing, not a claim of hardware rendering.
pub fn drm_report() -> Value {
    match inspect_drm() {
        Ok(report) => report,
        Err(error) => json!({"schema":1,"passed":false,"scope":"virtio_gpu_driver_only",
            "hardware_rendering_verified":false,"error":format!("{error:#}")}),
    }
}

fn inspect_drm() -> Result<Value> {
    use rustix::ioctl::{Opcode, Updater, ioctl, opcode};
    ensure!(std::process::id() == 1, "DRM probe requires guest PID1");
    let modules = load_modules()?;
    let mut nodes = Vec::new();
    let mut observed = Vec::new();
    for entry in fs::read_dir("/sys/class/drm")? {
        let entry = entry?;
        observed.push(json!({"name":entry.file_name().to_string_lossy(),
            "driver":fs::read_link(entry.path().join("device/driver")).ok()}));
        if !entry.file_name().as_encoded_bytes().starts_with(b"renderD") {
            continue;
        }
        let node = Path::new("/dev/dri").join(entry.file_name());
        let file = OpenOptions::new().read(true).write(true).open(&node)?;
        if drm_driver_name(&file)? != "virtio_gpu" {
            continue;
        }
        let parameter = |param: u64| -> Result<u64> {
            // Linux uapi drm/virtgpu_drm.h: DRM_COMMAND_BASE + GETPARAM, two __u64 fields, the second pointing to a __u64 result.
            const GETPARAM: Opcode = opcode::read_write::<[u64; 2]>(b'd', 0x43);
            let mut value = 0_u64;
            let mut request = [param, (&mut value as *mut u64) as u64];
            // SAFETY: DRM's version query identified this node as virtio_gpu.
            unsafe { ioctl(&file, Updater::<GETPARAM, _>::new(&mut request)) }
                .context("query guest virtio-GPU parameter")?;
            Ok(value)
        };
        let capsets = parameter(7)?;
        nodes.push(
            json!({"node":node,"driver":"virtio_gpu","resource_blob":parameter(3)?,
            "context_init":parameter(6)?,"supported_capsets":capsets,
            "drm_native_context_capset":capsets & (1 << 6) != 0}),
        );
    }
    ensure!(
        !nodes.is_empty(),
        "virtio-GPU has no usable guest render node; observed DRM entries: {observed:?}"
    );
    Ok(
        json!({"schema":1,"passed":true,"scope":"virtio_gpu_driver_only",
        "hardware_rendering_verified":false,"loaded_modules":modules,"render_nodes":nodes}),
    )
}

fn inspect_vulkan() -> Result<Value> {
    ensure!(
        std::process::id() == 1,
        "guest graphics initialization requires PID1"
    );
    // These are the disposable guest's virtual devices, not host input nodes.
    if Path::new("/dev/input").is_dir() {
        use std::os::unix::fs::FileTypeExt;
        for entry in fs::read_dir("/dev/input")? {
            let entry = entry?;
            if entry.file_name().as_encoded_bytes().starts_with(b"event")
                && entry.file_type()?.is_char_device()
            {
                fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o666))?;
            }
        }
    }
    record_stage("mounting guest userspace")?;
    for tag in ["usr", "frames"] {
        let target = format!("/{tag}");
        fs::create_dir_all(&target)?;
        mount(
            tag,
            target.as_str(),
            "virtiofs",
            if tag == "usr" {
                MountFlags::RDONLY
            } else {
                MountFlags::empty()
            },
            None,
        )
        .with_context(|| format!("mount guest share {tag}"))?;
    }
    record_stage("mounting shared directories and graphics libraries")?;
    // Mount only explicitly exported extra directories under a dedicated root.
    let shares = Path::new("/frames").join(legion_shared_directories::MANIFEST);
    if shares.exists() {
        ensure!(
            fs::metadata(&shares)?.len() <= legion_shared_directories::MAX_MANIFEST_BYTES,
            "guest share manifest exceeds 64 KiB"
        );
        let manifest: legion_shared_directories::Manifest =
            serde_json::from_slice(&fs::read(shares)?)?;
        manifest.validate()?;
        let mut mounted = Vec::new();
        for share in manifest.mounts {
            let target = share.target();
            fs::create_dir_all(&target)?;
            mount(
                share.tag.as_str(),
                &target,
                "virtiofs",
                if share.readonly {
                    MountFlags::RDONLY
                } else {
                    MountFlags::empty()
                },
                None,
            )
            .with_context(|| format!("mount additional guest share {}", share.tag))?;
            let readonly = rustix::fs::statvfs(target.as_path())?
                .f_flag
                .contains(rustix::fs::StatVfsMountFlags::RDONLY);
            ensure!(
                readonly == share.readonly,
                "share {} mount permission mismatch",
                share.tag
            );
            mounted.push(legion_shared_directories::Mounted {
                tag: share.tag,
                target,
                readonly,
            });
        }
        fs::write("/frames/mounted-shares.json", serde_json::to_vec(&mounted)?)?;
    }
    // Development userspace layout.
    for (name, target) in [
        ("/lib", "usr/lib"),
        ("/lib64", "usr/lib"),
        ("/bin", "usr/bin"),
        ("/sbin", "usr/bin"),
    ] {
        symlink(target, name).with_context(|| format!("create guest {name} link"))?;
    }
    fs::create_dir_all("/tmp")?;
    fs::set_permissions("/tmp", fs::Permissions::from_mode(0o1777))?;
    fs::create_dir_all("/run/user/0")?;
    fs::set_permissions("/run/user/0", fs::Permissions::from_mode(0o700))?;
    fs::create_dir_all("/dev/shm")?;
    mount(
        "tmpfs",
        "/dev/shm",
        "tmpfs",
        MountFlags::empty(),
        Some(c"mode=1777,size=64m"),
    )?;
    let expected = selected_gpu()?;
    let info = run_vulkan_program("/usr/bin/vulkaninfo", &["--summary"], "vulkaninfo")?;
    if let Err(error) =
        super::gpu_kind::verify_enumeration(&fs::read_to_string("/proc/cmdline")?, &info)
    {
        return Ok(
            json!({"schema": 1, "passed": false, "enumeration_verified": false, "draw_verified": false,
                "expected_vendor":expected.vendor_id(),"selected_icd":expected.icd(),
                "vulkaninfo": info, "error": error.to_string()}),
        );
    }
    fs::write(
        "/frames/vulkan-enumeration.json",
        serde_json::to_vec(&json!({
            "schema":1,"passed":true,"enumeration_verified":true,"draw_verified":false,
            "vulkaninfo":info,"expected_vendor":expected.vendor_id()
        }))?,
    )?;
    let draw = if Path::new("/probe-draw").is_file() {
        Some(run_vulkan_program(
            "/probe-draw",
            &["/frames/triangle.ppm"],
            "draw",
        )?)
    } else {
        None
    };
    let draw_verified = draw.as_ref().is_some_and(|value| {
        value["success"] == true
            && value["stdout"]
                .as_str()
                .is_some_and(|text| text.contains("red=11858 blue=53678 other=0"))
    });
    let passed = draw.is_none() || draw_verified;
    let display = if Path::new("/probe-display").is_file() {
        Some(run_vulkan_program("/probe-display", &[], "display")?)
    } else {
        None
    };
    let passed = passed
        && display
            .as_ref()
            .is_none_or(|value| value["success"] == true);
    let workload = if Path::new("/probe-workload").is_file() {
        Some(run_vulkan_program(
            "/probe-workload",
            &["/workload-config.json", "/frames/workload-result.json"],
            "workload",
        )?)
    } else {
        None
    };
    let mut workload_gpu_verified = false;
    let passed = if let Some(run) = &workload {
        ensure!(run["success"] == true, "workload process failed: {run}");
        let result = fs::read("/frames/workload-result.json").context("read workload result")?;
        ensure!(
            result.len() <= 8 * 1024 * 1024,
            "workload result exceeds 8 MiB"
        );
        let result: Value = serde_json::from_slice(&result)?;
        workload_gpu_verified = result["gpu_verified"] == true;
        passed && run["success"] == true && result["schema"] == 1 && result["passed"] == true
    } else {
        passed
    };
    Ok(json!({
        "schema": 1, "passed": passed, "enumeration_verified": true,
        "draw_verified": draw_verified, "vulkaninfo": info, "draw": draw,

        "display": display,
        "workload": workload, "workload_gpu_verified": workload_gpu_verified,
        "interrupts": interrupt_report()?,
        "idle_driver": read_sysfs("/sys/devices/system/cpu/cpuidle/current_driver"),
        "idle_governor": read_sysfs("/sys/devices/system/cpu/cpuidle/current_governor_ro"),
        "haltpoll_ns": read_sysfs("/sys/module/haltpoll/parameters/guest_halt_poll_ns"),
        "cpu_environment": cpu_environment(),
        "error": if passed { None::<&str> } else { Some("offscreen draw or pixel verification failed") },
    }))
}

// Observational metadata, outside the workload's timed interval.
fn read_sysfs(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

/// Identify the DRM implementation; a virtio render node's sysfs parent can name its PCI transport rather than its graphics driver.
fn drm_driver_name(file: &File) -> Result<String> {
    use rustix::ioctl::{Opcode, Updater, ioctl, opcode};

    #[repr(C)]
    struct DrmVersion {
        major: i32,
        minor: i32,
        patch: i32,
        _pad: u32,
        name_len: usize,
        name: *mut u8,
        date_len: usize,
        date: *mut u8,
        description_len: usize,
        description: *mut u8,
    }
    const VERSION: Opcode = opcode::read_write::<DrmVersion>(b'd', 0);
    let mut driver_name = [0_u8; 32];
    let mut version = DrmVersion {
        major: 0,
        minor: 0,
        patch: 0,
        _pad: 0,
        name_len: driver_name.len(),
        name: driver_name.as_mut_ptr(),
        date_len: 0,
        date: std::ptr::null_mut(),
        description_len: 0,
        description: std::ptr::null_mut(),
    };
    // SAFETY: VERSION is the standard x86-64 DRM version query.
    unsafe { ioctl(file, Updater::<VERSION, _>::new(&mut version)) }
        .context("identify DRM render-node driver")?;
    ensure!(
        version.name_len <= driver_name.len(),
        "DRM driver name exceeds buffer"
    );
    String::from_utf8(driver_name[..version.name_len].to_vec()).map_err(Into::into)
}

/// Record the guest CPU environment alongside rendering diagnostics.
fn cpu_environment() -> Value {
    let mut caches = std::collections::BTreeSet::new();
    if let Ok(cpus) = fs::read_dir("/sys/devices/system/cpu") {
        for cpu in cpus.flatten() {
            let name = cpu.file_name();
            let Some(id) = name.to_str().and_then(|s| s.strip_prefix("cpu")) else {
                continue;
            };
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let Ok(indices) = fs::read_dir(cpu.path().join("cache")) else {
                continue;
            };
            for index in indices.flatten() {
                let read = |name: &str| {
                    fs::read_to_string(index.path().join(name))
                        .ok()
                        .map(|s| s.trim().to_owned())
                };
                if let (Some(level), Some(kind), Some(size), Some(shared_cpus)) = (
                    read("level"),
                    read("type"),
                    read("size"),
                    read("shared_cpu_list"),
                ) {
                    caches.insert((level, kind, size, shared_cpus));
                }
            }
        }
    }
    let caches: Vec<_> = caches.into_iter().map(|(level, kind, size, shared_cpus)| {
        json!({"level":level,"type":kind,"size":size,"shared_cpu_list":shared_cpus})
    }).collect();
    json!({
        "cache_groups":caches,
        "nohz_full":read_sysfs("/sys/devices/system/cpu/nohz_full"),
        "cmdline":read_sysfs("/proc/cmdline"),
        "sched_bore":read_sysfs("/proc/sys/kernel/sched_bore"),
        "hypervisor_cpuid":super::cpuid(0x4000_0000, 0),
        "kvm_features_cpuid":super::cpuid(0x4000_0001, 0),
    })
}

/// Read the actual guest interrupt allocation, rather than infer it from QEMU options.
fn interrupt_report() -> Result<Value> {
    let mut devices = Vec::new();
    let mut names = Vec::new();
    for entry in fs::read_dir("/sys/bus/virtio/devices")? {
        let entry = entry?;
        if fs::read_link(entry.path().join("driver"))
            .ok()
            .as_ref()
            .and_then(|path| path.file_name())
            != Some(std::ffi::OsStr::new("virtio-gpu-nv"))
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = fs::canonicalize(entry.path())?;
        let parent = path
            .parent()
            .context("virtio device has no transport parent")?;
        let pci = parent.join("config").is_file();
        let mut vectors = Vec::new();
        match fs::read_dir(parent.join("msi_irqs")) {
            Ok(entries) => {
                for irq in entries {
                    let irq = irq?;
                    vectors.push(json!({
                        "irq": irq.file_name().to_string_lossy().parse::<u32>()?,
                        "kind": fs::read_to_string(irq.path())?.trim(),
                    }));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        vectors.sort_by_key(|value| value["irq"].as_u64());
        devices.push(json!({"device": name, "transport": if pci { "pci" } else { "mmio" }, "vectors": vectors}));
        names.push(name);
    }
    let interrupts = fs::read_to_string("/proc/interrupts")?;
    let lines = interrupts
        .lines()
        .filter(|line| names.iter().any(|name| line.contains(name)))
        .collect::<Vec<_>>();
    Ok(json!({"devices": devices, "irq_lines": lines}))
}

fn run_vulkan_program(program: &str, args: &[&str], name: &str) -> Result<Value> {
    record_stage(name)?;
    let stdout = format!("/frames/{name}.stdout");
    let stderr = format!("/frames/{name}.stderr");
    let status = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin")
        .env("HOME", "/tmp")
        .env("XDG_RUNTIME_DIR", "/run/user/0")
        .env("LD_LIBRARY_PATH", "/usr/lib")
        .env("VK_DRIVER_FILES", selected_gpu()?.icd())
        .env("__EGL_VENDOR_LIBRARY_DIRS", "/usr/share/glvnd/egl_vendor.d")
        .env("VK_LOADER_DEBUG", "error,warn")
        .stdin(Stdio::null())
        .stdout(File::create(&stdout)?)
        .stderr(File::create(&stderr)?)
        .status()
        .with_context(|| format!("run guest {program}"))?;
    let read = |path: &str| -> Result<String> {
        use std::io::Read;
        let mut bytes = Vec::new();
        File::open(path)?.take(64 * 1024).read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    let kernel_messages = if status.signal().is_some() {
        let mut bytes = vec![0_u8; 64 * 1024];
        // SAFETY: SYSLOG_ACTION_READ_ALL writes at most the allocated buffer length.
        let length = unsafe { libc::klogctl(3, bytes.as_mut_ptr().cast(), 65536) };
        if length > 0 {
            String::from_utf8_lossy(&bytes[..length as usize]).into_owned()
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    Ok(
        json!({"success": status.success(), "exit_code": status.code(), "signal": status.signal(), "stdout": read(&stdout)?, "stderr": read(&stderr)?, "kernel_messages": kernel_messages}),
    )
}

fn load_modules() -> Result<Vec<String>> {
    ensure!(std::process::id() == 1, "guest driver probe requires PID1");
    // PID1 mounted proc/sys/dev before recording the CPU topology proof.
    let mut modules = Vec::new();
    for name in fs::read_to_string("/modules/load-order")?.lines() {
        ensure!(
            !name.is_empty()
                && name.bytes().all(|byte| byte.is_ascii_digit()
                    || byte == b'.'
                    || byte == b'k'
                    || byte == b'o'),
            "invalid module manifest entry"
        );
        let file = File::open(Path::new("/modules").join(name))?;
        // This loads into the disposable guest kernel, never the host kernel.
        rustix::system::finit_module(&file, c"", 0)
            .with_context(|| format!("load guest module {name}"))?;
        modules.push(name.to_owned());
    }
    Ok(modules)
}
