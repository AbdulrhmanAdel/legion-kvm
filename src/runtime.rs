use std::{
    ffi::OsString,
    fs,
    os::unix::process::CommandExt,
    path::{Component, Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::{gpu_transport, memory, probe, qemu_policy};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub nvidia_driver: String,
    pub qemu: PathBuf,
    pub qemu_data: PathBuf,
    pub backend: PathBuf,
    pub virtiofsd: PathBuf,
    pub host_loader: PathBuf,
    pub host_libraries: PathBuf,
    pub kernel: PathBuf,
    pub init: PathBuf,
    pub modules: Vec<PathBuf>,
    pub userspace: PathBuf,
    pub workload_config: PathBuf,
    #[serde(default)]
    pub host_driver_libraries: bool,
    #[serde(default)]
    pub amdgpu: Option<AmdGpu>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AmdGpu {
    pub qemu: PathBuf,
    pub kernel: PathBuf,
    pub init: PathBuf,
    pub modules: Vec<PathBuf>,
    pub userspace: PathBuf,
    pub host_loader: PathBuf,
    pub host_libraries: PathBuf,
}

pub struct Runtime {
    pub root: PathBuf,
    pub manifest: Manifest,
}

#[derive(Serialize)]
pub struct Check {
    component: &'static str,
    passed: bool,
    detail: String,
}

#[derive(Serialize)]
pub struct Report {
    schema: u32,
    pub passed: bool,
    scope: &'static str,
    checks: Vec<Check>,
}

impl Runtime {
    pub fn load(root: Option<&Path>) -> Result<Self> {
        let root = match root {
            Some(root) => root.to_owned(),
            None => std::env::current_exe()?
                .parent()
                .and_then(Path::parent)
                .context("Legion must be inside the runtime's bin directory")?
                .to_owned(),
        }
        .canonicalize()
        .context("locate runtime directory")?;
        let manifest: Manifest = serde_json::from_slice(
            &fs::read(root.join("runtime.json"))
                .context("runtime.json is missing; extract the complete Legion test bundle")?,
        )?;
        ensure!(manifest.schema == 1, "unsupported runtime manifest schema");
        ensure!(
            !manifest.modules.is_empty(),
            "runtime contains no guest modules"
        );
        let runtime = Self { root, manifest };
        let m = &runtime.manifest;
        for path in [
            &m.qemu,
            &m.qemu_data,
            &m.backend,
            &m.virtiofsd,
            &m.host_loader,
            &m.host_libraries,
            &m.kernel,
            &m.init,
            &m.userspace,
            &m.workload_config,
        ] {
            runtime.asset(path)?;
        }
        for path in &m.modules {
            runtime.asset(path)?;
        }
        if let Some(amd) = &m.amdgpu {
            ensure!(
                !amd.modules.is_empty(),
                "AMD runtime contains no guest modules"
            );
            for path in [
                &amd.qemu,
                &amd.kernel,
                &amd.init,
                &amd.userspace,
                &amd.host_loader,
                &amd.host_libraries,
            ]
            .into_iter()
            .chain(&amd.modules)
            {
                runtime.asset(path)?;
            }
        }
        Ok(runtime)
    }

    pub(crate) fn asset(&self, relative: &Path) -> Result<PathBuf> {
        ensure!(
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|p| matches!(p, Component::Normal(_))),
            "runtime asset must be a relative path"
        );
        let path = self
            .root
            .join(relative)
            .canonicalize()
            .with_context(|| format!("missing runtime asset {}", relative.display()))?;
        ensure!(
            path.starts_with(&self.root),
            "runtime asset escapes the bundle"
        );
        Ok(path)
    }

    pub(crate) fn tool(&self, name: &str) -> PathBuf {
        self.root.join("bin").join(name)
    }

    pub fn inspect(&self) -> Report {
        let graphics = crate::graphics::detect();
        let amd = matches!(&graphics, Ok(crate::graphics::Graphics::Amd(_)));
        let mut checks = Vec::new();
        let mut check = |component, result: Result<String>| {
            checks.push(Check {
                component,
                passed: result.is_ok(),
                detail: match result {
                    Ok(detail) => detail,
                    Err(error) => format!("{error:#}"),
                },
            });
        };
        check(
            "kvm_and_cpu",
            (|| {
                let topology = crate::cpu::host_topology()?;
                let caps = legion_kvm_capabilities::inspect(Path::new("/dev/kvm"))?;
                let issues = caps.cpu_probe_issues(topology.qemu_slots()?);
                ensure!(issues.is_empty(), "{}", issues.join("; "));
                ensure!(
                    caps.fixed_backing_prerequisites_present(),
                    "KVM synchronous MMU support is missing"
                );
                Ok(format!(
                    "{} cores / {} threads",
                    topology.physical_cores, topology.logical_cpus
                ))
            })(),
        );
        check("graphics", graphics.map(|graphics| format!("{graphics:?}")));
        check(
            "graphics_driver",
            (|| {
                if amd {
                    ensure!(
                        self.manifest.amdgpu.is_some(),
                        "this runtime has no AMD graphics assets"
                    );
                    return Ok("AMDGPU native context; installed host graphics driver".into());
                }
                let version = crate::driver_libraries::loaded_version()?;
                if !self.manifest.host_driver_libraries {
                    ensure!(
                        version == self.manifest.nvidia_driver,
                        "embedded NVIDIA libraries require the matching loaded driver {}; this runtime has no host-library bindings",
                        self.manifest.nvidia_driver
                    );
                }
                Ok(format!(
                    "{version}; {}",
                    if self.manifest.host_driver_libraries {
                        "installed host libraries; backend selects driver ABI at runtime"
                    } else {
                        "embedded driver libraries"
                    }
                ))
            })(),
        );
        check(
            "desktop",
            (|| {
                let directory =
                    std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is missing")?;
                let display = std::env::var_os("WAYLAND_DISPLAY")
                    .context("a Wayland desktop is required for this alpha")?;
                let path = PathBuf::from(directory).join(display);
                use std::os::unix::fs::FileTypeExt;
                ensure!(
                    path.metadata()?.file_type().is_socket(),
                    "Wayland display is not a socket"
                );
                Ok("Wayland desktop is available".into())
            })(),
        );
        for (name, flag) in [
            (
                if amd {
                    "legion-amdgpu-qemu"
                } else {
                    "legion-qemu"
                },
                "--version",
            ),
            ("legion-virtiofsd", "--version"),
            ("legion-backend", "--help"),
        ] {
            if amd && name == "legion-backend" {
                continue;
            }
            check(
                name,
                (|| {
                    let output = Command::new(self.tool(name)).arg(flag).output()?;
                    ensure!(
                        output.status.success(),
                        "tool failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    Ok(String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .next()
                        .unwrap_or("available")
                        .to_owned())
                })(),
            );
        }
        Report {
            schema: 1,
            passed: checks.iter().all(|c| c.passed),
            scope: "Host CPU, graphics, desktop and runtime prerequisites",
            checks,
        }
    }
}

/// Executed through bundle-local symlinks to the same Rust launcher binary.
pub fn exec_tool(kind: &str, args: &[OsString]) -> Result<()> {
    let runtime = Runtime::load(None)?;
    let m = &runtime.manifest;
    let (asset, private_loader) = match kind {
        "legion-qemu" => (&m.qemu, true),
        "legion-amdgpu-qemu" => (
            &m.amdgpu.as_ref().context("runtime has no AMD QEMU")?.qemu,
            true,
        ),
        "legion-virtiofsd" => (&m.virtiofsd, true),
        "legion-backend" => (&m.backend, true),
        _ => bail!("unknown runtime tool"),
    };
    let binary = runtime.asset(asset)?;
    let mut command = if private_loader {
        let (loader, libraries) = if kind == "legion-amdgpu-qemu" {
            let amd = m
                .amdgpu
                .as_ref()
                .context("runtime has no AMD host libraries")?;
            (&amd.host_loader, &amd.host_libraries)
        } else {
            (&m.host_loader, &m.host_libraries)
        };
        let mut command = Command::new(runtime.asset(loader)?);
        command
            .arg("--library-path")
            .arg(runtime.asset(libraries)?)
            .arg(&binary);
        command
    } else {
        Command::new(&binary)
    };
    if matches!(kind, "legion-qemu" | "legion-amdgpu-qemu") {
        command
            .arg("-L")
            .arg(runtime.asset(&m.qemu_data)?)
            .env("LEGION_FIXED_SHMEM", "auto");
    } else if kind == "legion-backend" {
        command
            .args([
                "--shm-uncached-mib",
                "32",
                "--shm-write-combine-mib",
                "1024",
                "--shm-write-back-mib",
                "2016",
            ])
            .env("LEGION_FENCE_PROBE", "1");
        for name in [
            "LEGION_DIAG_RAW_WINDOW_LENGTHS",
            "LEGION_RUSD_CACHE_CONTROL",
            "LEGION_QUIET_POLL_ARM",
        ] {
            command.env_remove(name);
        }
    }
    command
        .args(args)
        .env_remove("LD_PRELOAD")
        .env_remove("LD_LIBRARY_PATH");
    Err(command.exec()).with_context(|| format!("execute runtime tool {kind}"))
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    let status = Command::new("cp")
        .args(["-a", "--reflink=auto", "--"])
        .arg(source)
        .arg(target)
        .status()
        .context("copy private runtime files (coreutils cp required)")?;
    ensure!(status.success(), "private copy failed");
    Ok(())
}

fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", path.display()))
}

pub fn benchmark(args: &[OsString]) -> Result<()> {
    let mut runtime_root = None;
    let (mut proton, mut application, mut output, mut state) = (None, None, None, None);
    let mut executable = OsString::from("Benchmark.exe");
    let mut hugepages = qemu_policy::HugePages::default();
    let mut memory_mib = 12288;
    let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let (options, remainder) = args[..at].as_chunks::<2>();
    for pair in options {
        let path = Some(PathBuf::from(&pair[1]));
        match pair[0].to_str() {
            Some("--runtime") => runtime_root = path,
            Some("--proton") => proton = path,
            Some("--application") => application = path,
            Some("--output") => output = path,
            Some("--state") => state = path,
            Some("--exe") => executable = pair[1].clone(),
            Some("--qemu-hugepages") => hugepages = qemu_policy::HugePages::parse(&pair[1])?,
            Some("--memory-mib") => {
                memory_mib = pair[1].to_str().context("invalid memory size")?.parse()?
            }
            _ => bail!("unknown benchmark option {:?}", pair[0]),
        }
    }
    ensure!(remainder.is_empty(), "every benchmark option needs a value");
    ensure!(
        (4096..=65536).contains(&memory_mib),
        "benchmark memory must be 4096..65536 MiB"
    );
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    ensure!(uid != 0 && gid != 0, "run Legion as your desktop user");
    let runtime = Runtime::load(runtime_root.as_deref())?;
    let report = runtime.inspect();
    if !report.passed {
        eprintln!("{}", serde_json::to_string_pretty(&report)?);
        bail!("runtime prerequisites failed; no VM started");
    }
    let proton = proton.context("--proton is required")?.canonicalize()?;
    ensure!(
        proton.join("proton").is_file(),
        "selected Proton has no proton entrypoint"
    );
    let application = application
        .context("--application is required")?
        .canonicalize()?;
    let executable_path = Path::new(&executable);
    let executable_text = executable
        .to_str()
        .context("benchmark executable path must be UTF-8")?;
    ensure!(
        !executable_path.as_os_str().is_empty()
            && executable_path
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "benchmark executable must be a relative path"
    );
    let exe = application.join(executable_path).canonicalize()?;
    ensure!(
        exe.starts_with(&application) && exe.is_file(),
        "executable escapes the application directory"
    );
    let available_mib = fs::read_to_string("/proc/meminfo")?
        .lines()
        .find_map(|line| {
            line.strip_prefix("MemAvailable:")?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
        .context("available host memory is unavailable")?
        / 1024;
    ensure!(
        available_mib >= memory_mib + 2048,
        "{available_mib} MiB available; this launch needs {} MiB headroom. Try --memory-mib 8192 or close heavy applications",
        memory_mib + 2048
    );
    let output = output.context("--output is required (a new result directory)")?;
    let state =
        state.context("--state is required (a private persistent prefix/cache directory)")?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let output = parent.canonicalize()?.join(
        output
            .file_name()
            .context("result directory needs a name")?,
    );
    ensure!(
        !output.starts_with(&application)
            && !output.starts_with(&proton)
            && !output.starts_with(&runtime.root),
        "results must be outside the installed application, Proton and runtime"
    );
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&output)
        .context("result directory must not already exist")?;
    let output = output.canonicalize()?;
    save(&output.join("preflight.json"), &report)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&state)?;
    let state = state.canonicalize()?;
    ensure!(
        !state.starts_with(&proton)
            && !state.starts_with(&application)
            && !state.starts_with(&runtime.root),
        "state must be separate from the installed Proton/application/runtime"
    );
    eprintln!("legion: Preparing private runtime and benchmark files…");
    copy_tree(
        &runtime.asset(&runtime.manifest.userspace)?,
        &output.join("usr"),
    )?;
    copy_tree(&application, &output.join("application"))?;
    let fixture = output.join("usr/share/legion");
    let arguments: Vec<String> =
        if at == args.len() {
            ["-d3d12", "-ResX=1920", "-ResY=1080", "-fullscreen", "-ForceRes", "-NoVSync",
         "-ExecCmds=r.VSync 0,r.ScreenPercentage 100,r.DynamicRes.OperationMode 0,t.MaxFPS 0"]
            .into_iter().map(str::to_owned).collect()
        } else {
            args[at + 1..]
                .iter()
                .map(|a| {
                    a.to_str()
                        .context("benchmark arguments must be UTF-8")
                        .map(str::to_owned)
                })
                .collect::<Result<_>>()?
        };
    ensure!(
        arguments.len() <= 16 && arguments.iter().all(|a| a.len() <= 512),
        "benchmark arguments exceed limits"
    );
    save(
        &fixture.join("launch.json"),
        &serde_json::json!({
            "proton_path":"/opt/legion-proton", "uid":uid, "gid":gid, "external_benchmark":true
        }),
    )?;
    save(
        &fixture.join("external-benchmark.json"),
        &serde_json::json!({
            "schema":1,"appid":0,"full_run":true,"application_share":true,
        "executable":executable_text,"arguments":arguments,"startup_seconds":240,
            "diagnostic_stop_on_device_loss":true,"forward_wait":"spin",
            "forward_pacing":"callbacks","forward_fullscreen":true
        }),
    )?;
    fs::write(
        fixture.join("etc/passwd"),
        format!(
            "root:x:0:0:root:/root:/bin/sh\nlegion:x:{uid}:{gid}:legion:/frames/home:/bin/sh\n"
        ),
    )?;
    fs::write(
        fixture.join("etc/group"),
        format!("root:x:0:\nlegion:x:{gid}:\n"),
    )?;
    let mut shares = vec![
        legion_shared_directories::Source {
            tag: "proton-tool".into(),
            directory: proton,
            readonly: true,
        },
        legion_shared_directories::Source {
            tag: "application".into(),
            directory: output.join("application"),
            readonly: false,
        },
    ];
    let driver_libraries = if runtime.manifest.host_driver_libraries {
        crate::driver_libraries::stage(&output.join("usr"), &mut shares)?
    } else {
        Default::default()
    };
    for (tag, name) in [
        ("proton-home", "home"),
        ("proton-compatdata", "compatdata"),
        ("proton-cache", "cache"),
    ] {
        let directory = state.join(name);
        fs::create_dir_all(&directory)?;
        shares.push(legion_shared_directories::Source {
            tag: tag.into(),
            directory,
            readonly: false,
        });
    }
    save(
        &output.join("shares.json"),
        &serde_json::json!({"schema":1,"directories":shares}),
    )?;
    let config = serde_json::from_slice(&fs::read(
        runtime.asset(&runtime.manifest.workload_config)?,
    )?)?;
    let boot = gpu_transport::DriverBoot {
        kernel: runtime.asset(&runtime.manifest.kernel)?,
        init: runtime.asset(&runtime.manifest.init)?,
        modules: runtime
            .manifest
            .modules
            .iter()
            .map(|p| runtime.asset(p))
            .collect::<Result<_>>()?,
        ram_mode: memory::Mode::Aligned,
        qemu_hugepages: hugepages,
        memory_mib: Some(memory_mib),
        vulkan: Some(gpu_transport::VulkanBoot {
            compute: true,
            userspace: output.join("usr"),
            virtiofsd: runtime.tool("legion-virtiofsd"),
            draw: None,
            presentation: None,
            forward_display: Some((1920, 1080)),
            desktop_audio: false,
            shares,
            driver_libraries,
            workload: Some(gpu_transport::WorkloadBoot {
                program: fixture.join("bootstrap"),
                config,
                timeout_seconds: Some(300),
                interactive: false,
            }),
        }),
    };
    let result = gpu_transport::run(
        &runtime.tool("legion-backend"),
        &runtime.tool("legion-qemu"),
        &output.join("vm"),
        probe::Machine::Q35,
        Some(&boot),
    )?;
    ensure!(
        result.error.is_none(),
        "benchmark VM failed: {}",
        result.error.as_deref().unwrap_or("unknown")
    );
    let bridge: serde_json::Value = serde_json::from_slice(&fs::read(
        output.join("vm/frames/proton-forward-result.json"),
    )?)?;
    let frames = bridge["frames_forwarded"]
        .as_u64()
        .context("missing forwarded frame count")?;
    ensure!(
        bridge["passed"] == true
            && bridge["animation_verified"] == true
            && frames > 0
            && bridge["host_buffer_releases"].as_u64() == Some(frames)
            && bridge["acquire_fences_pending_at_shutdown"] == 0
            && bridge["steady_state_cpu_maps"] == 0
            && bridge["full_frame_cpu_transport"] == false,
        "GPU animation/ownership checks failed; inspect the forwarding report"
    );
    ensure!(
        output
            .join("application/BenchmarkResultsPerFrame.csv")
            .is_file(),
        "application exited without BenchmarkResultsPerFrame.csv; inspect VM logs"
    );
    save(
        &output.join("summary.json"),
        &serde_json::json!({
            "schema":1,"functional_checks_passed":true,"performance_comparison_qualified":false,
            "forwarded_frames":frames,"qemu_hugepages":hugepages,"guest_cpu":result.guest_cpu,
            "timing_scope":"Benchmark-reported tick timing; visibility and a matched native run must be checked separately"
        }),
    )?;
    eprintln!("legion: Benchmark complete. Results: {}", output.display());
    Ok(())
}

pub fn doctor(args: &[OsString]) -> Result<()> {
    let root = match args {
        [] => None,
        [flag, path] if flag == "--runtime" => Some(Path::new(path)),
        _ => bail!("expected runtime-doctor [--runtime <bundle directory>]"),
    };
    let report = Runtime::load(root)?.inspect();
    println!("{}", serde_json::to_string_pretty(&report)?);
    ensure!(report.passed, "runtime prerequisites failed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path) -> Runtime {
        let manifest = serde_json::from_value(serde_json::json!({
            "schema":1,"nvidia_driver":"1.2.3","qemu":"owned","qemu_data":"owned",
            "backend":"owned","virtiofsd":"owned","host_loader":"owned",
            "host_libraries":"owned","kernel":"owned","init":"owned",
            "modules":["owned"],"userspace":"owned","workload_config":"owned"
        }))
        .expect("fixture manifest");
        Runtime {
            root: root.to_owned(),
            manifest,
        }
    }

    #[test]
    fn runtime_assets_reject_parent_absolute_and_symlink_escapes() {
        let directory = tempfile::tempdir().expect("private fixture");
        let root = directory.path().join("bundle");
        fs::create_dir(&root).expect("bundle root");
        fs::write(root.join("owned"), b"fixture").expect("owned asset");
        fs::write(directory.path().join("outside"), b"outside").expect("outside asset");
        std::os::unix::fs::symlink(directory.path().join("outside"), root.join("escape"))
            .expect("escape fixture");
        let runtime = fixture(&root);
        assert_eq!(
            runtime.asset(Path::new("owned")).expect("owned"),
            root.join("owned")
        );
        for value in ["../outside", "/etc/passwd", "escape", ""] {
            assert!(runtime.asset(Path::new(value)).is_err(), "{value}");
        }
    }
}
