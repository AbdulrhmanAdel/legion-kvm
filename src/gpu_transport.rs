//! Paused negotiation or a booted, disposable guest-driver test.
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use legion_cpu_topology::Topology;
use serde::Serialize;

use crate::{
    archive, cpu, memory, presentation,
    probe::{self, Machine},
    process::{CoreDumpLimit, OwnedChild},
    startup::{Progress, Stage},
};

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: u32,
    pub backend: PathBuf,
    pub backend_compute: bool,
    pub desktop_audio_requested: bool,
    pub interactive_session: bool,
    pub machine: Machine,
    pub transport_ready: bool,
    pub guest_device_probe_verified: bool,
    pub device_mappings_verified: bool,
    pub hardware_vulkan_verified: bool,
    pub vulkan_enumeration_verified: bool,
    pub guest_vulkan: Option<serde_json::Value>,
    pub presentation_verified: bool,
    pub presentation: Option<serde_json::Value>,
    pub complete_profile_verified: bool,
    pub cpu_topology: Topology,
    pub boot_cpu: String,
    pub boot_kernel: Option<PathBuf>,
    pub guest_cpu: Option<probe::GuestReport>,
    pub guest_driver: Option<serde_json::Value>,
    pub guest_memory: Option<memory::Info>,
    pub qemu_hugepages: crate::qemu_policy::HugePages,
    pub elapsed_ms: u128,
    pub backend_log: String,
    pub qemu_log: String,
    pub error: Option<String>,
}

pub struct DriverBoot {
    pub kernel: PathBuf,
    pub init: PathBuf,
    /// Uncompressed .ko files in dependency order; loaded only in the guest.
    pub modules: Vec<PathBuf>,
    pub vulkan: Option<VulkanBoot>,
    pub ram_mode: memory::Mode,
    pub qemu_hugepages: crate::qemu_policy::HugePages,
    pub memory_mib: Option<u64>,
}

pub struct VulkanBoot {
    /// Expose the backend's existing UVM route for D3D12 driver initialization.
    pub compute: bool,
    pub userspace: PathBuf,
    pub virtiofsd: PathBuf,
    pub draw: Option<PathBuf>,
    pub presentation: Option<presentation::Config>,
    pub workload: Option<WorkloadBoot>,

    pub forward_display: Option<(u32, u32)>,
    pub desktop_audio: bool,
    pub shares: Vec<legion_shared_directories::Source>,
    pub driver_libraries: legion_shared_directories::LibraryBindings,
}

pub struct WorkloadBoot {
    pub program: PathBuf,
    pub config: legion_workload_probe::Config,
    pub timeout_seconds: Option<u64>,
    pub interactive: bool,
}

pub fn run(
    backend: &Path,
    qemu: &Path,
    output: &Path,
    machine: Machine,
    boot: Option<&DriverBoot>,
) -> Result<Report> {
    ensure!(!backend.as_os_str().is_empty(), "--backend is required");
    ensure!(!output.as_os_str().is_empty(), "--output is required");
    let backend = fs::canonicalize(backend).context("locate vhost-user-nvgpu")?;
    ensure!(
        boot.is_none() || matches!(machine, Machine::Q35),
        "guest NVIDIA tests require q35: microvm's 512-byte virtio-MMIO window cannot contain this device's config"
    );
    let topology = cpu::host_topology().context("detect host CPU topology")?;
    let boot_cpu = cpu::boot_cpu()?;
    let _core_limit = CoreDumpLimit::disabled()?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(output)
        .context("create GPU probe directory (must not exist)")?;
    let output = fs::canonicalize(output)?;
    let mut progress = Progress::new(&output, boot.is_some());
    // Keep Unix socket names short even when the output directory is long.
    let sockets = tempfile::Builder::new().prefix("legion-gpu-").tempdir()?;
    let socket = sockets.path().join("nv.sock");
    let qmp = sockets.path().join("qmp.sock");
    let backend_log = output.join("backend.log");
    let qemu_log = output.join("qemu.log");
    let console = output.join("console.log");
    let prepared = boot.map(|boot| prepare_boot(boot, &output)).transpose()?;
    let start = Instant::now();
    let mut transport_ready = false;
    let mut guest_cpu = None;
    let mut guest_driver = None;
    let mut guest_vulkan = None;
    let vulkan = boot.and_then(|boot| boot.vulkan.as_ref());
    let backend_compute = vulkan.is_some_and(|v| v.compute);
    let timeout_seconds = vulkan.and_then(|v| v.workload.as_ref()).map_or(60, |w| {
        w.timeout_seconds
            .unwrap_or_else(|| w.config.timeout_seconds())
    });
    let interactive = vulkan
        .and_then(|v| v.workload.as_ref())
        .is_some_and(|w| w.interactive);
    let session_deadline = if interactive {
        crate::session::Deadline::InteractiveStartup(Duration::from_secs(timeout_seconds))
    } else {
        crate::session::Deadline::Bounded(Duration::from_secs(timeout_seconds))
    };
    let mut presentation_result = None;
    let mut guest_memory = None;

    let outcome = (|| -> Result<()> {
        progress.set(Stage::Graphics);
        let backend_file = File::create(&backend_log)?;
        let mut server_command = Command::new(&backend);
        if let Some(config) = vulkan
            .and_then(|v| v.presentation.as_ref())
            .filter(|config| config.gpu_forward)
        {
            server_command
                .arg("--legion-display")
                .arg(format!("{}x{}", config.width, config.height));
        } else if let Some((width, height)) = vulkan.and_then(|v| v.forward_display) {
            server_command
                .arg("--legion-display")
                .arg(format!("{width}x{height}"));
        }
        let mut server = OwnedChild(
            server_command
                .arg("--socket")
                .arg(&socket)
                .args([
                    "--caps",
                    if backend_compute {
                        "graphics,compute,video,utility"
                    } else {
                        "graphics,video,utility"
                    },
                ])
                .env("RUST_LOG", "info")
                .env_remove("LD_PRELOAD")
                .stdin(Stdio::null())
                .stdout(backend_file.try_clone()?)
                .stderr(backend_file)
                .spawn()
                .context("start NVIDIA backend")?,
        );
        loop {
            if socket.metadata().is_ok_and(|m| m.file_type().is_socket()) {
                break;
            }
            if let Some(status) = server.0.try_wait()? {
                bail!("NVIDIA backend exited before creating its socket: {status}");
            }
            ensure!(
                start.elapsed() < Duration::from_secs(10),
                "NVIDIA backend socket timed out"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // The custom protocol uses virtio ID 45, two queues, and 4016 bytes of guest-readable config; a virtio-gpu device (ID 16) is different.
        let (machine_arg, device) = match machine {
            Machine::Q35 => (
                "q35,accel=kvm,memory-backend=ram,smm=off",
                "vhost-user-test-device-pci",
            ),
            Machine::Microvm => (
                "microvm,accel=kvm,memory-backend=ram",
                "vhost-user-test-device",
            ),
        };
        // QEMU key/value options escape literal commas by doubling them.
        let nv_arg = format!("socket,id=nv,path={}", qemu_path(&socket)?);
        let qmp_arg = format!("unix:{},server=on,wait=off", qemu_path(&qmp)?);
        let mut device_arg =
            format!("{device},virtio-id=45,num_vqs=2,vq_size=256,config_size=4016,chardev=nv");
        if matches!(machine, Machine::Q35) {
            // Separate control and completion interrupts.
            device_arg.push_str(",vectors=3");
        }
        let mut command = crate::qemu_policy::command(
            qemu,
            boot.map_or(crate::qemu_policy::HugePages::default(), |b| {
                b.qemu_hugepages
            }),
        )?;
        let mut fs_servers = Vec::new();
        if let Some(vulkan) = vulkan {
            ensure!(
                vulkan.userspace.join("bin/vulkaninfo").is_file(),
                "--userspace must contain bin/vulkaninfo"
            );
            let frames = output.join("frames");
            fs::create_dir(&frames)?;
            let extra = legion_shared_directories::Sources {
                schema: 1,
                directories: vulkan.shares.clone(),
            }
            .resolve()?;
            vulkan.driver_libraries.validate(&extra.manifest())?;
            serde_json::to_writer(
                File::create(frames.join(legion_shared_directories::LIBRARY_MANIFEST))?,
                &vulkan.driver_libraries,
            )?;
            serde_json::to_writer(
                File::create(frames.join(legion_shared_directories::MANIFEST))?,
                &extra.manifest(),
            )?;
            if let Some(config) = vulkan
                .presentation
                .as_ref()
                .filter(|config| config.gpu_forward)
            {
                serde_json::to_writer(
                    File::create(frames.join("presentation-config.json"))?,
                    &serde_json::json!({"schema":1,"gpu_forward":true,"width":config.width,"height":config.height,"seconds":config.seconds,"uncapped":config.uncapped,"acquire_mode":config.acquire_mode.unwrap_or(crate::presentation::AcquireMode::Async),"frame_transport":config.frame_transport.unwrap_or(crate::presentation::FrameTransport::Separate),"frame_pacing":config.frame_pacing.unwrap_or_default()}),
                )?;
            }
            let standard = [
                ("usr", vulkan.userspace.as_path(), true),
                ("frames", frames.as_path(), false),
            ];
            for (tag, directory, readonly) in standard.into_iter().chain(
                extra
                    .directories
                    .iter()
                    .map(|s| (s.tag.as_str(), s.directory.as_path(), s.readonly)),
            ) {
                let fs_socket = sockets.path().join(format!("{tag}.sock"));
                let log = File::create(output.join(format!("virtiofs-{tag}.log")))?;
                let mut fs_command = Command::new(&vulkan.virtiofsd);
                fs_command
                    .arg("--shared-dir")
                    .arg(directory)
                    .arg("--socket-path")
                    .arg(&fs_socket)
                    .args(["--sandbox", "namespace"])
                    .env_remove("LD_PRELOAD")
                    .stdin(Stdio::null())
                    .stdout(log.try_clone()?)
                    .stderr(log);
                if readonly {
                    fs_command.arg("--readonly");
                }
                let mut child = OwnedChild(fs_command.spawn().context("start virtiofsd")?);
                let deadline = Instant::now();
                while !fs_socket
                    .metadata()
                    .is_ok_and(|m| m.file_type().is_socket())
                {
                    ensure!(
                        child.0.try_wait()?.is_none(),
                        "virtiofsd exited; inspect virtiofs-{tag}.log"
                    );
                    ensure!(
                        deadline.elapsed() < Duration::from_secs(10),
                        "virtiofsd socket timed out"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                command
                    .arg("-chardev")
                    .arg(format!(
                        "socket,id=fs-{tag},path={}",
                        qemu_path(&fs_socket)?
                    ))
                    .arg("-device")
                    .arg(format!("vhost-user-fs-pci,chardev=fs-{tag},tag={tag}"));
                fs_servers.push(child);
            }
        }
        let memory = boot
            .and_then(|b| b.memory_mib)
            .unwrap_or(if vulkan.is_some() { 2048 } else { 512 });
        ensure!(
            (512..=65536).contains(&memory),
            "guest memory must be 512..65536 MiB"
        );
        let memory_text = memory.to_string();
        let ram = memory::Ram::new(boot.map_or(memory::Mode::default(), |b| b.ram_mode), memory)?;
        guest_memory = Some(ram.info.clone());
        let mut display = vulkan
            .and_then(|v| v.presentation.as_ref())
            .filter(|config| !config.gpu_forward)
            .map(|config| presentation::Session::start(config, &output, &mut command))
            .transpose()?;
        command
            .args([
                "-machine",
                machine_arg,
                "-cpu",
                &boot_cpu,
                "-m",
                &memory_text,
            ])
            .args(topology.qemu_args(&boot_cpu)?)
            .args(["-object", &ram.argument])
            .args([
                "-nodefaults",
                "-no-reboot",
                "-display",
                "none",
                "-monitor",
                "none",
                "-nic",
                "none",
            ])
            .args([
                "-chardev",
                &nv_arg,
                "-device",
                &device_arg,
                "-qmp",
                &qmp_arg,
            ])
            .env_remove("LD_PRELOAD")
            .stdin(Stdio::null())
            .stdout(File::create(&console)?)
            .stderr(File::create(&qemu_log)?);
        crate::audio::configure(&mut command, vulkan.is_some_and(|v| v.desktop_audio));
        if let Some((kernel, initramfs)) = &prepared {
            command
                .arg("-kernel")
                .arg(kernel)
                .arg("-initrd")
                .arg(initramfs)
                .args([
                    "-serial",
                    "stdio",
                    "-append",
                    if vulkan.is_some() {
                        "console=ttyS0 rdinit=/init quiet panic=-1 legion.vulkan=1"
                    } else {
                        "console=ttyS0 rdinit=/init quiet panic=-1"
                    },
                ]);
        } else {
            command.arg("-S");
        }
        progress.set(Stage::Booting);
        let mut vm = OwnedChild(command.spawn().context("start QEMU GPU probe")?);
        loop {
            progress.poll();
            if let Some(display) = display.as_mut() {
                display.check()?;
            }
            if prepared.is_some()
                && let Some(driver) = read_driver_report(&console)?
            {
                guest_cpu = probe::read_report(&console)?;
                guest_driver = Some(driver);
                guest_cpu
                    .as_ref()
                    .context("guest omitted CPU report")?
                    .verify(&topology)?;
                verify_driver(
                    guest_driver
                        .as_ref()
                        .context("guest omitted driver report")?,
                )?;
                progress.set(Stage::GuestReady);
                if vulkan.is_some() {
                    guest_vulkan = read_json_report(&console, b"LEGION_VULKAN_PROBE ")?;
                    if let Some(report) = &guest_vulkan {
                        ensure!(report["schema"] == 1, "unknown guest Vulkan report schema");
                        ensure!(
                            report["passed"] == true,
                            "guest Vulkan test failed: {}",
                            report["error"]
                        );
                        ensure!(
                            report["enumeration_verified"] == true,
                            "guest omitted hardware enumeration proof"
                        );
                        if let Some(config) = vulkan.filter(|v| !v.shares.is_empty()) {
                            let path = output.join("frames/mounted-shares.json");
                            ensure!(fs::metadata(&path).context("guest did not acknowledge directory shares; use a compatible init")?.len()
                                    <= legion_shared_directories::MAX_MANIFEST_BYTES, "guest share acknowledgement exceeds 64 KiB");
                            let mounted: Vec<legion_shared_directories::Mounted> =
                                serde_json::from_slice(&fs::read(path)?)?;
                            legion_shared_directories::Sources {
                                schema: 1,
                                directories: config.shares.clone(),
                            }
                            .manifest()
                            .verify_mounted(&mounted)?;
                        }
                        if vulkan.is_some_and(|v| v.forward_display.is_some()) {
                            let forwarded: serde_json::Value = serde_json::from_slice(&fs::read(
                                output.join("frames/proton-forward-result.json"),
                            )?)?;
                            ensure!(
                                forwarded["passed"] == true
                                    && forwarded["mode"] == "vm_dmabuf_forward"
                                    && forwarded["full_frame_cpu_transport"] == false,
                                "owned Proton desktop forwarding failed"
                            );
                            presentation_result = Some(forwarded);
                        }
                        if vulkan.is_some_and(|v| v.draw.is_some()) {
                            ensure!(report["draw_verified"] == true, "guest omitted draw proof");
                            ensure!(
                                output.join("frames/triangle.ppm").is_file(),
                                "guest did not export the rendered frame"
                            );
                        }
                        if let Some(display) = display.as_mut() {
                            ensure!(
                                report["display"]["success"] == true,
                                "guest display producer failed: {}",
                                report["display"]
                            );
                            presentation_result = Some(display.finish()?);
                        }
                        if vulkan
                            .and_then(|v| v.presentation.as_ref())
                            .is_some_and(|config| config.gpu_forward)
                        {
                            ensure!(
                                report["display"]["success"] == true,
                                "guest GPU forwarding failed: {}",
                                report["display"]
                            );
                            let text = report["display"]["stdout"]
                                .as_str()
                                .context("guest omitted forwarding result")?;
                            let view: serde_json::Value = serde_json::from_str(text.trim())
                                .context("parse GPU forwarding result")?;
                            ensure!(
                                view["passed"] == true
                                    && view["mode"] == "vm_dmabuf_forward"
                                    && view["full_frame_cpu_transport"] == false,
                                "GPU forwarding verification failed"
                            );
                            presentation_result = Some(view);
                        }
                    } else {
                        ensure!(
                            vm.0.try_wait()?.is_none(),
                            "QEMU exited before the Vulkan result"
                        );
                        ensure!(
                            server.0.try_wait()?.is_none(),
                            "NVIDIA backend exited before the Vulkan result"
                        );
                        ensure!(
                            !session_deadline
                                .expired(start.elapsed(), progress.rendering_started()),
                            "Vulkan test timed out"
                        );
                        std::thread::sleep(Duration::from_millis(25));
                        continue;
                    }
                }
                vm.stop()?;
                server.stop_after_grace(Duration::from_millis(250))?;
                return Ok(());
            }
            if let Some(status) = vm.0.try_wait()? {
                bail!("QEMU device initialization failed: {status}");
            }
            if let Some(status) = server.0.try_wait()? {
                bail!("NVIDIA backend exited during device negotiation: {status}");
            }
            if !transport_ready && let Ok(mut monitor) = UnixStream::connect(&qmp) {
                monitor.set_read_timeout(Some(Duration::from_millis(250)))?;
                let mut greeting = Vec::new();
                let read = (&mut monitor).take(4096).read_to_end(&mut greeting);
                if !greeting.is_empty()
                    && let Some(line) = greeting.split(|byte| *byte == b'\n').next()
                    && let Ok(value) = serde_json::from_slice::<serde_json::Value>(line)
                    && value.get("QMP").is_some()
                {
                    transport_ready = true;
                    if prepared.is_none() {
                        vm.stop()?;
                        server.stop_after_grace(Duration::from_millis(250))?;
                        return Ok(());
                    }
                }
                if let Err(error) = read {
                    ensure!(
                        matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ),
                        "read QMP greeting: {error}"
                    );
                }
            }
            ensure!(
                if prepared.is_some() {
                    !session_deadline.expired(start.elapsed(), progress.rendering_started())
                } else {
                    start.elapsed() < Duration::from_secs(15)
                },
                "GPU probe timed out"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    })();
    if prepared.is_some() && console.is_file() {
        if guest_cpu.is_none() {
            guest_cpu = probe::read_report(&console)?;
        }
        if guest_driver.is_none() {
            guest_driver = read_driver_report(&console)?;
        }
        if guest_vulkan.is_none() {
            guest_vulkan = read_json_report(&console, b"LEGION_VULKAN_PROBE ")?;
        }
    }
    let presentation_path = output.join("presentation.json");
    if presentation_result.is_none() && presentation_path.is_file() {
        presentation_result = Some(
            fs::read(&presentation_path)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
                .unwrap_or_else(
                    |error| serde_json::json!({"passed": false, "error": format!("{error:#}")}),
                ),
        );
    }
    let presentation_verified = outcome.is_ok()
        && presentation_result
            .as_ref()
            .is_some_and(|report| report["passed"] == true);
    let rendered = guest_vulkan.as_ref().is_some_and(|v| {
        v["passed"] == true
            && (v["draw_verified"] == true
                || v["workload_gpu_verified"] == true
                || (presentation_verified && v["display"]["success"] == true))
    });
    let report = Report {
        schema: 1,
        backend,
        backend_compute,
        desktop_audio_requested: vulkan.is_some_and(|v| v.desktop_audio),
        interactive_session: interactive,
        machine,
        transport_ready,
        guest_device_probe_verified: guest_driver
            .as_ref()
            .is_some_and(|report| verify_driver(report).is_ok()),
        device_mappings_verified: rendered,
        hardware_vulkan_verified: rendered,
        vulkan_enumeration_verified: guest_vulkan
            .as_ref()
            .is_some_and(|v| v["enumeration_verified"] == true),
        guest_vulkan,
        presentation_verified,
        presentation: presentation_result,
        complete_profile_verified: false,
        cpu_topology: topology,
        boot_cpu,
        boot_kernel: prepared.as_ref().map(|(kernel, _)| kernel.clone()),
        guest_cpu,
        guest_driver,
        guest_memory,
        qemu_hugepages: boot.map_or(crate::qemu_policy::HugePages::default(), |b| {
            b.qemu_hugepages
        }),
        elapsed_ms: start.elapsed().as_millis(),
        backend_log: log_tail(&backend_log)?,
        qemu_log: log_tail(&qemu_log)?,
        error: outcome.err().map(|e| format!("{e:#}")),
    };
    serde_json::to_writer_pretty(File::create(output.join("report.json"))?, &report)?;
    if let Some(error) = &report.error {
        progress.set(Stage::Failed);
        bail!("{error}; inspect {}", output.join("report.json").display());
    }
    progress.set(Stage::Complete);
    Ok(report)
}

fn prepare_boot(boot: &DriverBoot, output: &Path) -> Result<(PathBuf, PathBuf)> {
    ensure!(!boot.kernel.as_os_str().is_empty(), "--kernel is required");
    ensure!(!boot.init.as_os_str().is_empty(), "--init is required");
    ensure!(
        !boot.modules.is_empty() && boot.modules.len() <= 64,
        "provide 1..=64 --module paths in guest dependency order"
    );
    let kernel = fs::canonicalize(&boot.kernel).context("locate guest kernel")?;
    ensure!(kernel.is_file(), "guest kernel must be a file");
    ensure!(
        boot.init.metadata()?.len() <= 32 * 1024 * 1024,
        "guest init exceeds 32 MiB"
    );
    let init = fs::read(&boot.init).context("read static guest init")?;
    probe::validate_static_elf(&init)?;
    let mut modules = Vec::with_capacity(boot.modules.len());
    let mut total = 0_u64;
    for path in &boot.modules {
        let size = path
            .metadata()
            .with_context(|| format!("stat guest module {}", path.display()))?
            .len();
        ensure!(size <= 32 * 1024 * 1024, "guest module exceeds 32 MiB");
        total = total
            .checked_add(size)
            .context("guest module size overflow")?;
        ensure!(total <= 128 * 1024 * 1024, "guest modules exceed 128 MiB");
        let bytes = fs::read(path)?;
        ensure!(
            bytes.starts_with(b"\x7fELF"),
            "{} must be an uncompressed .ko file",
            path.display()
        );
        modules.push(bytes);
    }
    let initramfs = output.join("initramfs.cpio");
    let draw = boot
        .vulkan
        .as_ref()
        .and_then(|v| v.draw.as_ref())
        .map(fs::read)
        .transpose()?;
    if let Some(draw) = &draw {
        ensure!(
            draw.len() <= 32 * 1024 * 1024 && draw.starts_with(b"\x7fELF"),
            "draw probe must be an ELF of at most 32 MiB"
        );
    }
    let display = boot
        .vulkan
        .as_ref()
        .and_then(|v| v.presentation.as_ref())
        .map(|p| fs::read(&p.guest))
        .transpose()?;
    if let Some(bytes) = &display {
        ensure!(
            bytes.len() <= 32 * 1024 * 1024 && bytes.starts_with(b"\x7fELF"),
            "guest display must be an ELF of at most 32 MiB"
        );
    }
    let workload = boot.vulkan.as_ref().and_then(|v| v.workload.as_ref());
    let workload_bytes = workload.map(|w| fs::read(&w.program)).transpose()?;
    if let Some(bytes) = &workload_bytes {
        ensure!(
            bytes.len() <= 32 * 1024 * 1024 && bytes.starts_with(b"\x7fELF"),
            "workload must be an ELF of at most 32 MiB"
        );
    }
    let config_bytes = workload
        .map(|w| serde_json::to_vec(&w.config))
        .transpose()?;
    archive::write_with_workload(
        File::create(&initramfs)?,
        &init,
        &modules,
        draw.as_deref(),
        display.as_deref(),
        workload_bytes.as_deref(),
        config_bytes.as_deref(),
    )?;
    Ok((kernel, initramfs))
}

fn read_driver_report(console: &Path) -> Result<Option<serde_json::Value>> {
    read_json_report(console, b"LEGION_GPU_PROBE ")
}

fn read_json_report(console: &Path, prefix: &[u8]) -> Result<Option<serde_json::Value>> {
    ensure!(
        console.metadata()?.len() <= 4 * 1024 * 1024,
        "guest console exceeds 4 MiB"
    );
    let bytes = fs::read(console)?;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if line.last() == Some(&b'\n')
            && let Some(json) = line.strip_prefix(prefix)
        {
            return Ok(Some(
                serde_json::from_slice(json).context("parse guest GPU report")?,
            ));
        }
    }
    Ok(None)
}

fn verify_driver(report: &serde_json::Value) -> Result<()> {
    ensure!(report["schema"] == 1, "unknown guest GPU report schema");
    for field in [
        "passed",
        "driver_bound",
        "nvidia_control_open",
        "nvidia_device_open",
    ] {
        ensure!(
            report[field] == true,
            "guest GPU test failed at {field}: {}",
            report["error"]
        );
    }
    ensure!(
        report["render_nodes"]
            .as_array()
            .is_some_and(|nodes| !nodes.is_empty()),
        "guest GPU test omitted render nodes"
    );
    Ok(())
}

pub(crate) fn qemu_path(path: &Path) -> Result<String> {
    Ok(path
        .to_str()
        .context("QEMU socket path must be UTF-8")?
        .replace(',', ",,"))
}

fn log_tail(path: &Path) -> Result<String> {
    use std::io::{Seek, SeekFrom};
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error.into()),
    };
    let size = file.metadata()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(64 * 1024)))?;
    let mut bytes = Vec::new();
    file.take(64 * 1024).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
