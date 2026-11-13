use super::*;

#[test]
#[ignore = "requires LEGION_DRM_TEST_QEMU, KERNEL, INIT, MODULES and accessible KVM"]
fn virtio_gpu_guest_modules_bind_and_report_capsets() -> Result<()> {
    let env_path = |name| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .with_context(|| format!("set {name}"))
    };
    let modules: Vec<PathBuf> = serde_json::from_str(&std::env::var("LEGION_DRM_TEST_MODULES")?)?;
    let modules = modules
        .iter()
        .map(fs::read)
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(!modules.is_empty(), "guest modules are required");
    let directory = tempfile::tempdir()?;
    let initrd = directory.path().join("initramfs.cpio");
    archive::write_with_modules(
        File::create(&initrd)?,
        &fs::read(env_path("LEGION_DRM_TEST_INIT")?)?,
        &modules,
    )?;
    let console = directory.path().join("console.log");
    let stderr = directory.path().join("qemu.log");
    let topology = cpu::host_topology()?;
    let model = cpu::boot_cpu()?;
    let mut command = Command::new(env_path("LEGION_DRM_TEST_QEMU")?);
    command.arg("-L").arg(
        std::env::var_os("LEGION_DRM_TEST_QEMU_DATA").unwrap_or_else(|| "/usr/share/qemu".into()),
    );
    command
        .args([
            "-machine",
            "q35,accel=kvm,smm=off",
            "-cpu",
            &model,
            "-m",
            "512",
        ])
        .args(topology.qemu_args(&model)?)
        .args([
            "-nodefaults",
            "-no-reboot",
            "-display",
            "none",
            "-monitor",
            "none",
            "-serial",
            "stdio",
            "-nic",
            "none",
            "-device",
            "virtio-gpu-pci",
        ])
        .arg("-kernel")
        .arg(env_path("LEGION_DRM_TEST_KERNEL")?)
        .arg("-initrd")
        .arg(initrd)
        .args([
            "-append",
            "console=ttyS0 rdinit=/init quiet panic=-1 legion.drm_probe=1",
        ])
        .env_remove("LD_PRELOAD")
        .stdin(Stdio::null())
        .stdout(File::create(&console)?)
        .stderr(File::create(&stderr)?);
    let mut child = OwnedChild(command.spawn()?);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let text = fs::read_to_string(&console)?;
        if let Some(line) = text
            .lines()
            .find_map(|line| line.strip_prefix("LEGION_DRM_PROBE "))
        {
            let driver: serde_json::Value = serde_json::from_str(line)?;
            let cpu = read_report(&console)?.context("CPU report missing")?;
            child.stop()?;
            if let Some(path) = std::env::var_os("LEGION_DRM_TEST_REPORT") {
                fs::write(
                    path,
                    serde_json::to_vec_pretty(&serde_json::json!({
                        "cpu":cpu,"driver":driver,"console":text,"qemu_stderr":fs::read_to_string(&stderr)?
                    }))?,
                )?;
            }
            cpu.verify(&topology)?;
            ensure!(
                driver["passed"] == true,
                "virtio-GPU module probe failed: {driver}; guest console: {text}"
            );
            ensure!(
                driver["hardware_rendering_verified"] == false,
                "driver probe must not claim rendering"
            );
            return Ok(());
        }
        ensure!(
            child.0.try_wait()?.is_none(),
            "DRM test guest exited: {}",
            fs::read_to_string(&stderr)?
        );
        ensure!(
            Instant::now() < deadline,
            "DRM test guest timed out: {text}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
