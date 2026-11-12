use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use legion_cpu_topology::Topology;
use legion_kvm_capabilities::Capabilities;
use serde::Serialize;

use crate::{cpu, process::OwnedChild};

#[derive(Debug, Serialize)]
pub struct Check {
    pub component: &'static str,
    pub status: &'static str,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct HostReport {
    pub schema: u32,
    pub architecture: &'static str,
    pub kernel_release: String,
    pub cpu_probe_prerequisites_present: bool,
    pub game_launch_available: bool,
    pub cpu_topology: Option<Topology>,
    pub kvm_capabilities: Option<Capabilities>,
    pub fixed_backing_kernel_prerequisites_present: bool,
    pub checks: Vec<Check>,
    pub render_devices: Vec<RenderDevice>,
}

#[derive(Debug, Serialize)]
pub struct RenderDevice {
    pub node: PathBuf,
    pub pci_vendor: Option<String>,
    pub driver: Option<String>,
    pub read_write_access: bool,
    pub backend: &'static str,
    pub backend_status: &'static str,
}

pub fn inspect() -> Result<HostReport> {
    let kvm = legion_kvm_capabilities::inspect(Path::new("/dev/kvm"));
    let qemu = executable(OsStr::new("qemu-system-x86_64"));
    let qemu_result = qemu.as_ref().map(|path| tool_output(path, &["--version"]));
    let qemu_available = matches!(qemu_result, Some(Ok(_)));
    let mut checks = vec![Check {
        component: "kvm",
        status: if kvm.is_ok() {
            "queried"
        } else {
            "unavailable"
        },
        detail: match &kvm {
            Ok(caps) => {
                format!(
                    "KVM API {}; read-only system capability queries completed; cpu-probe verifies actual execution",
                    caps.api_version
                )
            }
            Err(error) => error.to_string(),
        },
    }];
    let cpu_topology = match cpu::host_topology() {
        Ok(topology) => {
            checks.push(Check {
                component: "cpu_topology",
                status: "present",
                detail: format!(
                    "{} sockets, {} physical cores, {} logical CPUs; QEMU -smp {}",
                    topology.sockets,
                    topology.physical_cores,
                    topology.logical_cpus,
                    topology.qemu_smp()?
                ),
            });
            Some(topology)
        }
        Err(error) => {
            checks.push(Check {
                component: "cpu_topology",
                status: "unsupported",
                detail: format!("{error:#}"),
            });
            None
        }
    };
    let mut kvm_ready = false;
    if let Ok(caps) = &kvm {
        let slots = cpu_topology
            .as_ref()
            .map(Topology::qemu_slots)
            .transpose()?
            .unwrap_or(0);
        let issues = caps.cpu_probe_issues(slots);
        kvm_ready = issues.is_empty();
        checks.push(Check {
            component: "kvm_cpu_probe_capabilities",
            status: if kvm_ready { "present" } else { "unsupported" },
            detail: if kvm_ready {
                "required x86 CPU-probe capabilities and vCPU limit are present".to_owned()
            } else {
                issues.join("; ")
            },
        });
        checks.push(Check {
            component: "fixed_backing_kernel_prerequisites",
            status: if caps.fixed_backing_prerequisites_present() { "present" } else { "unavailable" },
            detail: format!("user_memory={}, synchronous_mmu={}; kernel prerequisites only, GPU ABI and remapping still require a separate probe", caps.user_memory, caps.synchronous_mmu),
        });
    }
    checks.push(Check {
        component: "qemu",
        status: if qemu_available { "present" } else { "missing" },
        detail: match qemu_result {
            Some(Ok(version)) => version.lines().next().unwrap_or_default().to_owned(),
            Some(Err(error)) => format!("{error:#}"),
            None => "qemu-system-x86_64 was not found in PATH".to_owned(),
        },
    });
    let virtiofsd = executable(OsStr::new("virtiofsd"))
        .or_else(|| executable(OsStr::new("/usr/lib/virtiofsd")))
        .or_else(|| executable(OsStr::new("/usr/libexec/virtiofsd")));
    checks.push(Check {
        component: "virtiofsd",
        status: if virtiofsd.is_some() {
            "present"
        } else {
            "missing"
        },
        detail: virtiofsd.map_or_else(
            || "virtiofsd is needed for the planned shared filesystem".to_owned(),
            |path| path.display().to_string(),
        ),
    });
    let render_devices = render_devices()?;
    let has_nvidia = render_devices
        .iter()
        .any(|gpu| gpu.driver.as_deref() == Some("nvidia"));
    if has_nvidia {
        let backend = executable(OsStr::new("vhost-user-nvgpu"));
        checks.push(Check {
            component: "nvidia_backend",
            status: if backend.is_some() { "unverified" } else { "missing" },
            detail: backend.map_or_else(
                || "vhost-user-nvgpu is not installed; the generic QEMU GPU device does not substitute for it".to_owned(),
                |path| format!("{}; backend negotiation and driver ABI still need verification", path.display()),
            ),
        });
        checks.push(Check {
            component: "nvidia_driver",
            status: "unverified",
            detail: fs::read_to_string("/proc/driver/nvidia/version")
                .unwrap_or_else(|error| error.to_string())
                .trim()
                .to_owned(),
        });
    }
    checks.push(Check {
        component: "shared_gpu_and_display",
        status: "unverified",
        detail: "GPU acceleration requires the selected transport, matching guest modules and graphics libraries".to_owned(),
    });
    checks.push(Check {
        component: "portable_gpu_backends",
        status: "unimplemented",
        detail: "NVIDIA and AMD use separate runtime backends; Intel graphics is unsupported"
            .to_owned(),
    });
    checks.push(Check {
        component: "cpu_identity",
        status: "unimplemented",
        detail: "cpu-probe tests reduced XSTATE with host CPU identity; complete CPU profile verification is unavailable".to_owned(),
    });
    Ok(HostReport {
        schema: 2,
        architecture: std::env::consts::ARCH,
        kernel_release: fs::read_to_string("/proc/sys/kernel/osrelease")?
            .trim()
            .to_owned(),
        cpu_probe_prerequisites_present: kvm_ready
            && qemu_available
            && cpu_topology.is_some()
            && std::env::consts::ARCH == "x86_64",
        game_launch_available: false,
        cpu_topology,
        fixed_backing_kernel_prerequisites_present: kvm
            .as_ref()
            .is_ok_and(Capabilities::fixed_backing_prerequisites_present),
        kvm_capabilities: kvm.ok(),
        checks,
        render_devices,
    })
}

pub(crate) fn render_devices() -> Result<Vec<RenderDevice>> {
    let mut devices = Vec::new();
    let entries = match fs::read_dir("/sys/class/drm") {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(devices),
        Err(error) => return Err(error).context("list DRM devices"),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_name().as_encoded_bytes().starts_with(b"renderD") {
            continue;
        }
        let node = Path::new("/dev/dri").join(entry.file_name());
        let device = entry.path().join("device");
        let pci_vendor = fs::read_to_string(device.join("vendor"))
            .ok()
            .map(|s| s.trim().to_owned());
        let driver = fs::read_link(device.join("driver")).ok().and_then(|p| {
            p.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
        let read_write_access = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
            .is_ok();
        let (backend, backend_status) = backend_for_driver(driver.as_deref());
        devices.push(RenderDevice {
            node,
            pci_vendor,
            driver,
            read_write_access,
            backend,
            backend_status,
        });
    }
    devices.sort_by(|a, b| a.node.cmp(&b.node));
    Ok(devices)
}

fn backend_for_driver(driver: Option<&str>) -> (&'static str, &'static str) {
    match driver {
        Some("nvidia") => ("nvidia_abi", "requires_runtime"),
        Some("amdgpu") => ("amdgpu_native_context", "not_integrated"),
        Some("i915") => ("intel_i915_native_context", "not_integrated"),
        Some("xe") => ("intel_xe_native_context", "not_integrated"),
        _ => ("unsupported", "unsupported"),
    }
}

fn executable(name: &OsStr) -> Option<PathBuf> {
    let executable_file = |path: &Path| {
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    };
    let path = Path::new(name);
    if path.components().count() > 1 {
        return executable_file(path).then(|| path.to_owned());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| executable_file(candidate))
}

fn tool_output(program: &Path, args: &[&str]) -> Result<String> {
    let directory = tempfile::tempdir()?;
    let stdout = directory.path().join("stdout");
    let stderr = directory.path().join("stderr");
    let mut child = OwnedChild(
        Command::new(program)
            .args(args)
            .env_remove("LD_PRELOAD")
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout)?)
            .stderr(fs::File::create(&stderr)?)
            .spawn()
            .with_context(|| format!("start {}", program.display()))?,
    );
    let status = child.wait_until(Duration::from_secs(5))?;
    ensure!(
        status.success(),
        "{} failed: {}",
        program.display(),
        fs::read_to_string(stderr)?
    );
    ensure!(
        stdout.metadata()?.len() <= 1024 * 1024,
        "tool output exceeds 1 MiB"
    );
    fs::read_to_string(stdout).context("read tool output")
}
