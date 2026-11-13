//! AMDGPU native-context enumeration and rendering probes.
use crate::{
    archive, cpu, doctor,
    gpu_transport::qemu_path,
    memory, probe,
    process::{CoreDumpLimit, OwnedChild},
    qemu_policy,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs::{self, File},
    os::unix::fs::{DirBuilderExt, FileTypeExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Config {
    qemu: PathBuf,
    kernel: PathBuf,
    init: PathBuf,
    modules: Vec<PathBuf>,
    userspace: PathBuf,
    virtiofsd: PathBuf,
    output: PathBuf,
    render_node: Option<PathBuf>,
    workload: Option<PathBuf>,
    workload_config: Option<PathBuf>,
    presentation: Option<PathBuf>,
    presentation_config: Option<PathBuf>,
    window: bool,
    x11_window: bool,
    desktop_renderer: bool,
    shares: Option<PathBuf>,
    memory_mib: Option<u32>,
    interactive: bool,
    application: bool,
    desktop_audio: bool,
}

fn parse(args: &[OsString]) -> Result<Config> {
    let mut config = Config::default();
    let (pairs, remainder) = args.as_chunks::<2>();
    for pair in pairs {
        let path = PathBuf::from(&pair[1]);
        match pair[0].to_str() {
            Some("--qemu") => config.qemu = path,
            Some("--kernel") => config.kernel = path,
            Some("--init") => config.init = path,
            Some("--module") => config.modules.push(path),
            Some("--userspace") => config.userspace = path,
            Some("--virtiofsd") => config.virtiofsd = path,
            Some("--output") => config.output = path,
            Some("--render-node") => config.render_node = Some(path),
            Some("--workload") => config.workload = Some(path),
            Some("--workload-config") => config.workload_config = Some(path),
            Some("--presentation") => config.presentation = Some(path),
            Some("--presentation-config") => config.presentation_config = Some(path),
            Some("--shares") => config.shares = Some(path),
            Some("--memory-mib") => {
                let mib: u32 = pair[1].to_str().context("invalid memory size")?.parse()?;
                ensure!(
                    (1024..=65536).contains(&mib),
                    "memory must be 1024..65536 MiB"
                );
                config.memory_mib = Some(mib);
            }
            Some("--audio") => match pair[1].to_str() {
                Some("desktop") => config.desktop_audio = true,
                Some("off") => config.desktop_audio = false,
                _ => bail!("--audio must be desktop or off"),
            },
            Some("--window") => match pair[1].to_str() {
                Some("true") => config.window = true,
                Some("false") => config.window = false,
                _ => bail!("--window must be true or false"),
            },
            Some("--interactive" | "--application") => {
                let value = match pair[1].to_str() {
                    Some("true") => true,
                    Some("false") => false,
                    _ => bail!("session switch must be true or false"),
                };
                if pair[0] == "--interactive" {
                    config.interactive = value;
                } else {
                    config.application = value;
                }
            }
            Some("--window-system") => match pair[1].to_str() {
                Some("wayland") => config.x11_window = false,
                Some("x11") => config.x11_window = true,
                _ => bail!("--window-system must be wayland or x11"),
            },
            Some("--window-renderer") => match pair[1].to_str() {
                Some("desktop") => config.desktop_renderer = true,
                Some("render-node") => config.desktop_renderer = false,
                _ => bail!("--window-renderer must be desktop or render-node"),
            },
            _ => bail!("unknown native-context probe option: {:?}", pair[0]),
        }
    }
    ensure!(
        remainder.is_empty(),
        "native-context probe option requires a value"
    );
    for (name, path) in [
        ("qemu", &config.qemu),
        ("kernel", &config.kernel),
        ("init", &config.init),
        ("userspace", &config.userspace),
        ("virtiofsd", &config.virtiofsd),
        ("output", &config.output),
    ] {
        ensure!(!path.as_os_str().is_empty(), "--{name} is required");
    }
    ensure!(
        !config.modules.is_empty() && config.modules.len() <= 16,
        "supply 1..16 matching guest modules in dependency order"
    );
    ensure!(
        config.workload.is_some() == config.workload_config.is_some(),
        "--workload and --workload-config must be supplied together"
    );
    ensure!(
        config.presentation_config.is_none()
            || config.presentation.is_some()
            || config.workload.is_some(),
        "--presentation-config requires --presentation or --workload"
    );
    Ok(config)
}

fn select(devices: &[doctor::RenderDevice], requested: Option<&Path>) -> Result<PathBuf> {
    let mut candidates = devices.iter().filter(|device| {
        device.driver.as_deref() == Some("amdgpu")
            && device.read_write_access
            && requested.is_none_or(|path| path == device.node)
    });
    let device = candidates.next().context("no accessible AMDGPU render node; enable an AMD GPU or run this probe on an AMD GPU machine")?;
    ensure!(
        candidates.next().is_none(),
        "multiple AMD GPUs are available; select --render-node explicitly for this probe"
    );
    Ok(device.node.clone())
}

fn read_marker(console: &str, marker: &str) -> Result<Option<Value>> {
    console
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
        .find_map(|line| line.strip_prefix(marker))
        .map(serde_json::from_str)
        .transpose()
        .map_err(Into::into)
}

fn share(
    program: &Path,
    sockets: &Path,
    output: &Path,
    tag: &str,
    directory: &Path,
    readonly: bool,
    command: &mut Command,
) -> Result<OwnedChild> {
    let socket = sockets.join(format!("{tag}.sock"));
    let log = File::create(output.join(format!("virtiofs-{tag}.log")))?;
    let mut service = Command::new(program);
    service
        .arg("--shared-dir")
        .arg(directory)
        .arg("--socket-path")
        .arg(&socket)
        .args(["--sandbox", "namespace"])
        .env_remove("LD_PRELOAD")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if readonly {
        service.arg("--readonly");
    }
    let mut child = OwnedChild(service.spawn().context("start native-context virtiofsd")?);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket
        .metadata()
        .is_ok_and(|meta| meta.file_type().is_socket())
    {
        ensure!(
            child.0.try_wait()?.is_none(),
            "virtiofsd exited; see virtiofs-{tag}.log"
        );
        ensure!(Instant::now() < deadline, "virtiofsd socket timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
    command.args([
        "-chardev",
        &format!("socket,id=fs-{tag},path={}", qemu_path(&socket)?),
        "-device",
        &format!("vhost-user-fs-pci,chardev=fs-{tag},tag={tag}"),
    ]);
    Ok(child)
}

pub(crate) fn run(args: &[OsString]) -> Result<()> {
    let config = parse(args)?;
    let requested = config
        .render_node
        .as_ref()
        .map(fs::canonicalize)
        .transpose()?;
    let node = select(&doctor::render_devices()?, requested.as_deref())?;
    let topology = cpu::host_topology()?;
    let capabilities = legion_kvm_capabilities::inspect(Path::new("/dev/kvm"))?;
    ensure!(
        capabilities
            .cpu_probe_issues(topology.qemu_slots()?)
            .is_empty(),
        "KVM CPU prerequisites are missing"
    );
    let userspace = config.userspace.canonicalize()?;
    let shares = config
        .shares
        .as_ref()
        .map(|path| -> Result<_> {
            ensure!(
                fs::metadata(path)?.len() <= legion_shared_directories::MAX_MANIFEST_BYTES,
                "directory-share configuration exceeds 64 KiB"
            );
            let sources: legion_shared_directories::Sources =
                serde_json::from_slice(&fs::read(path)?)?;
            sources.resolve()
        })
        .transpose()?;
    ensure!(
        userspace.join("bin/vulkaninfo").is_file()
            && userspace
                .join("share/vulkan/icd.d/radeon_icd.json")
                .is_file(),
        "userspace must contain vulkaninfo and the prepared RADV ICD"
    );
    let kernel = config.kernel.canonicalize()?;
    let init = fs::read(&config.init)?;
    probe::validate_static_elf(&init)?;
    ensure!(init.len() <= 32 * 1024 * 1024, "guest init is too large");
    let modules = config
        .modules
        .iter()
        .map(|path| {
            ensure!(
                fs::metadata(path)?.len() <= 32 * 1024 * 1024,
                "guest module is too large"
            );
            fs::read(path).map_err(Into::into)
        })
        .collect::<Result<Vec<_>>>()?;
    let workload = config
        .workload
        .as_ref()
        .map(|path| {
            ensure!(
                fs::metadata(path)?.len() <= 32 * 1024 * 1024,
                "workload exceeds 32 MiB"
            );
            fs::read(path).map_err(Into::into)
        })
        .transpose()?;
    let workload_config = config
        .workload_config
        .as_ref()
        .map(|path| -> Result<_> {
            ensure!(
                fs::metadata(path)?.len() <= 64 * 1024,
                "workload configuration exceeds 64 KiB"
            );
            let bytes = fs::read(path)?;
            let settings: legion_workload_probe::Config = serde_json::from_slice(&bytes)?;
            settings.validate()?;
            Ok((bytes, settings.timeout_seconds()))
        })
        .transpose()?;
    let timeout_seconds = workload_config
        .as_ref()
        .map_or(60, |(_, timeout)| 60 + timeout);
    let presentation = config
        .presentation
        .as_ref()
        .map(|path| -> Result<_> {
            ensure!(
                fs::metadata(path)?.len() <= 32 * 1024 * 1024,
                "presentation program exceeds 32 MiB"
            );
            Ok(fs::read(path)?)
        })
        .transpose()?;
    fs::DirBuilder::new().mode(0o700).create(&config.output)?;
    let output = config.output.canonicalize()?;
    let frames = output.join("frames");
    fs::create_dir(&frames)?;
    if let Some(shares) = &shares {
        fs::write(
            frames.join(legion_shared_directories::MANIFEST),
            serde_json::to_vec_pretty(&shares.manifest())?,
        )?;
    }
    let mut dimensions = (1920_u32, 1080_u32);
    let mut fullscreen = false;
    if let Some(path) = &config.presentation_config {
        ensure!(
            fs::metadata(path)?.len() <= 64 * 1024,
            "presentation configuration exceeds 64 KiB"
        );
        let bytes = fs::read(path)?;
        let settings: Value = serde_json::from_slice(&bytes)?;
        ensure!(
            settings["schema"] == 1,
            "unknown presentation configuration schema"
        );
        if let (Some(w), Some(h)) = (settings["width"].as_u64(), settings["height"].as_u64()) {
            ensure!(
                (64..=3840).contains(&w) && (64..=2160).contains(&h),
                "invalid display dimensions"
            );
            dimensions = (u32::try_from(w)?, u32::try_from(h)?);
        }
        fullscreen = settings["fullscreen"] == true;
        fs::write(frames.join("kms-presentation.json"), bytes)?;
    }
    let initrd = output.join("initramfs.cpio");
    archive::write_with_workload(
        File::create(&initrd)?,
        &init,
        &modules,
        None,
        presentation.as_deref(),
        workload.as_deref(),
        workload_config.as_ref().map(|(bytes, _)| bytes.as_slice()),
    )?;
    let _core_limit = CoreDumpLimit::disabled()?;
    let sockets = tempfile::Builder::new().prefix("legion-drm-").tempdir()?;
    let memory_mib = config.memory_mib.unwrap_or(1024);
    let ram = memory::Ram::new(memory::Mode::Aligned, u64::from(memory_mib))?;
    let model = cpu::boot_cpu()?;
    let mut command = qemu_policy::command(&config.qemu, qemu_policy::HugePages::Off)?;
    command.args(["-machine","q35,accel=kvm,smm=off,memory-backend=ram","-cpu",&model,"-m",&memory_mib.to_string()])
        .args(topology.qemu_args(&model)?)
        .args(["-object",&ram.argument,"-nodefaults","-no-reboot","-monitor","none","-serial","stdio","-nic","none"])
        .args(["-display",&if config.window { if fullscreen { "sdl,gl=on,full-screen=on".to_owned() } else { "sdl,gl=on".to_owned() } } else { format!("egl-headless,rendernode={}",qemu_path(&node)?) },
            "-device",&format!("virtio-gpu-gl-pci,hostmem=1G,blob=on,drm_native_context=on,venus=off,xres={},yres={}{}",dimensions.0,dimensions.1,
                if config.desktop_renderer { format!(",drm-rendernode={}",qemu_path(&node)?) } else { String::new() })])
        .arg("-kernel").arg(kernel).arg("-initrd").arg(initrd)
        .args(["-append","console=ttyS0 rdinit=/init quiet panic=-1 legion.drm_probe=1 legion.vulkan=1 legion.gpu=amdgpu"])
        .env_remove("LD_PRELOAD").env_remove("GBM_BACKEND").env_remove("__EGL_VENDOR_LIBRARY_FILENAMES")
        .stdin(Stdio::null()).stdout(File::create(output.join("console.log"))?)
        .stderr(File::create(output.join("qemu.log"))?);
    crate::audio::configure(&mut command, config.desktop_audio);
    if config.window && !config.desktop_renderer {
        let pci = fs::canonicalize(
            Path::new("/sys/class/drm")
                .join(node.file_name().context("render node name missing")?)
                .join("device"),
        )?;
        let pci = pci
            .file_name()
            .and_then(|name| name.to_str())
            .context("GPU PCI address missing")?;
        command
            .env("DRI_PRIME", format!("pci-{}", pci.replace([':', '.'], "_")))
            .env(
                "__EGL_VENDOR_LIBRARY_FILENAMES",
                "/usr/share/glvnd/egl_vendor.d/50_mesa.json",
            )
            .env(
                "SDL_VIDEODRIVER",
                if config.x11_window { "x11" } else { "wayland" },
            );
        if config.x11_window {
            command.env("__GLX_VENDOR_LIBRARY_NAME", "mesa");
        }
    }
    if config.window && config.desktop_renderer {
        command
            .env("LEGION_SCANOUT_EXTERNAL", "1")
            .env_remove("DRI_PRIME")
            .env_remove("__GLX_VENDOR_LIBRARY_NAME")
            .env(
                "SDL_VIDEODRIVER",
                if config.x11_window { "x11" } else { "wayland" },
            );
    }
    let mut services = vec![
        share(
            &config.virtiofsd,
            sockets.path(),
            &output,
            "usr",
            &userspace,
            true,
            &mut command,
        )?,
        share(
            &config.virtiofsd,
            sockets.path(),
            &output,
            "frames",
            &frames,
            false,
            &mut command,
        )?,
    ];
    if let Some(shares) = &shares {
        for source in &shares.directories {
            services.push(share(
                &config.virtiofsd,
                sockets.path(),
                &output,
                &source.tag,
                &source.directory,
                source.readonly,
                &mut command,
            )?);
        }
    }
    fs::write(
        output.join("command.json"),
        serde_json::to_vec_pretty(&json!({
            "program":command.get_program(),"arguments":command.get_args().collect::<Vec<_>>(),"render_node":node
        }))?,
    )?;
    let mut child = OwnedChild(command.spawn().context("start AMD native-context QEMU")?);
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let mut report = json!({"schema":1,"passed":false,"render_node":node,
        "hardware_rendering_verified":false,"presentation_included":presentation.is_some() || config.presentation_config.is_some(),
        "desktop_capture_verified":false,
        "desktop_window_requested":config.window,
        "scope":if workload.is_some() { "amdgpu_native_context_rendering" } else { "amdgpu_native_context_enumeration" }});
    let result: Result<()> = (|| {
        loop {
            let path = output.join("console.log");
            ensure!(
                fs::metadata(&path)?.len() <= 1024 * 1024,
                "guest console exceeds probe bound"
            );
            let text = fs::read_to_string(&path)?;
            if let Some(driver) = read_marker(&text, "LEGION_DRM_PROBE ")? {
                report["driver"] = driver;
                ensure!(
                    report["driver"]["passed"] == true,
                    "guest virtio-GPU driver probe failed"
                );
                let nodes = report["driver"]["render_nodes"]
                    .as_array()
                    .context("missing guest render nodes")?;
                ensure!(
                    nodes
                        .iter()
                        .any(|node| node["drm_native_context_capset"] == true
                            && node["resource_blob"] == 1
                            && node["context_init"] == 1),
                    "guest lacks DRM native-context/blob capabilities"
                );
            }
            if let Some(vulkan) = read_marker(&text, "LEGION_VULKAN_PROBE ")? {
                report["vulkan"] = vulkan;
                let cpu = probe::read_report(&path)?.context("guest CPU report missing")?;
                cpu.verify(&topology)?;
                report["cpu"] = serde_json::to_value(cpu)?;
                ensure!(
                    report["driver"]["passed"] == true,
                    "guest driver proof is missing"
                );
                ensure!(
                    report["vulkan"]["passed"] == true
                        && report["vulkan"]["enumeration_verified"] == true,
                    "AMDGPU hardware Vulkan enumeration failed; inspect report.json and frames/vulkaninfo.stderr"
                );
                if workload.is_some() && !config.application {
                    ensure!(
                        report["vulkan"]["workload_gpu_verified"] == true,
                        "AMDGPU workload did not verify its rendered pixels"
                    );
                    report["hardware_rendering_verified"] = true.into();
                }
                report["passed"] = true.into();
                return Ok(());
            }
            if let Some(status) = child.0.try_wait()? {
                ensure!(
                    config.interactive && status.success(),
                    "QEMU exited; see qemu.log"
                );
                report["closed_by_user"] = true.into();
                report["passed"] = true.into();
                return Ok(());
            }
            ensure!(
                (config.interactive && frames.join("kms-first-frame.json").is_file())
                    || Instant::now() < deadline,
                "native-context session timed out"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    })();
    child.stop()?;
    drop(services);
    drop(ram);
    if let Err(error) = &result {
        report["error"] = format!("{error:#}").into();
    }
    fs::write(
        output.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    result
}

#[cfg(test)]
#[path = "../tests/unit/native_context.rs"]
mod tests;
