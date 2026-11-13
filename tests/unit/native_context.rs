use super::*;

#[test]
#[ignore = "requires LEGION_RADV_TEST_INPUTS, prepared RADV userspace and accessible KVM"]
fn radv_dependencies_load_in_guest_without_claiming_hardware_rendering() -> Result<()> {
    #[derive(serde::Deserialize)]
    struct Fixture {
        qemu: PathBuf,
        kernel: PathBuf,
        init: PathBuf,
        modules: Vec<PathBuf>,
        userspace: PathBuf,
        virtiofsd: PathBuf,
        report: PathBuf,
    }
    let inputs =
        std::env::var_os("LEGION_RADV_TEST_INPUTS").context("set LEGION_RADV_TEST_INPUTS")?;
    let input: Fixture = serde_json::from_slice(&fs::read(inputs)?)?;
    let directory = tempfile::tempdir()?;
    let frames = directory.path().join("frames");
    fs::create_dir(&frames)?;
    let initrd = directory.path().join("initramfs.cpio");
    let modules = input
        .modules
        .iter()
        .map(fs::read)
        .collect::<std::io::Result<Vec<_>>>()?;
    archive::write_with_modules(File::create(&initrd)?, &fs::read(&input.init)?, &modules)?;
    let ram = memory::Ram::new(memory::Mode::Aligned, 1024)?;
    let sockets = tempfile::Builder::new()
        .prefix("legion-radv-test-")
        .tempdir()?;
    let topology = cpu::host_topology()?;
    let model = cpu::boot_cpu()?;
    let console = directory.path().join("console.log");
    let error = directory.path().join("qemu.log");
    let mut command = Command::new(&input.qemu);
    command.args(["-L","/usr/share/qemu","-machine","q35,accel=kvm,smm=off,memory-backend=ram","-m","1024","-cpu",&model])
            .args(topology.qemu_args(&model)?)
            .args(["-object",&ram.argument,"-nodefaults","-no-reboot","-display","none","-monitor","none","-serial","stdio","-nic","none","-device","virtio-gpu-pci"])
            .arg("-kernel").arg(input.kernel).arg("-initrd").arg(initrd)
            .args(["-append","console=ttyS0 rdinit=/init quiet panic=-1 legion.drm_probe=1 legion.vulkan=1 legion.gpu=amdgpu"])
            .env_remove("LD_PRELOAD").stdin(Stdio::null()).stdout(File::create(&console)?)
            .stderr(File::create(&error)?);
    let services = vec![
        share(
            &input.virtiofsd,
            sockets.path(),
            directory.path(),
            "usr",
            &input.userspace,
            true,
            &mut command,
        )?,
        share(
            &input.virtiofsd,
            sockets.path(),
            directory.path(),
            "frames",
            &frames,
            false,
            &mut command,
        )?,
    ];
    let mut child = OwnedChild(command.spawn()?);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let text = fs::read_to_string(&console)?;
        if let Some(vulkan) = read_marker(&text, "LEGION_VULKAN_PROBE ")? {
            let driver = read_marker(&text, "LEGION_DRM_PROBE ")?.context("missing DRM report")?;
            let cpu = probe::read_report(&console)?.context("missing CPU report")?;
            child.stop()?;
            drop(services);
            let summary = json!({"schema":1,"scope":"radv_guest_library_negative_control",
                    "hardware_rendering_verified":false,"cpu":cpu,"driver":driver,"vulkan":vulkan,
                    "qemu_stderr":fs::read_to_string(&error)?});
            fs::write(input.report, serde_json::to_vec_pretty(&summary)?)?;
            cpu.verify(&topology)?;
            ensure!(driver["passed"] == true, "guest driver failed: {driver}");
            ensure!(
                vulkan["expected_vendor"] == "0x1002",
                "wrong GPU driver selected"
            );
            ensure!(
                vulkan["driver_dependency_check"]["success"] == true,
                "RADV dependencies failed: {vulkan}"
            );
            ensure!(
                vulkan["passed"] == false && vulkan["enumeration_verified"] == false,
                "2D negative control must not report AMD hardware rendering"
            );
            ensure!(
                vulkan["error"] == "selected hardware Vulkan enumeration failed",
                "unexpected RADV probe failure: {vulkan}"
            );
            return Ok(());
        }
        ensure!(
            child.0.try_wait()?.is_none(),
            "guest exited: {}",
            fs::read_to_string(&error)?
        );
        ensure!(
            Instant::now() < deadline,
            "RADV guest test timed out: {text}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn serial_reports_wait_for_complete_lines() -> Result<()> {
    assert!(read_marker("LEGION_DRM_PROBE {\"passed\":tr", "LEGION_DRM_PROBE ")?.is_none());
    assert_eq!(
        read_marker(
            "boot\nLEGION_DRM_PROBE {\"passed\":true}\r\n",
            "LEGION_DRM_PROBE "
        )?,
        Some(json!({"passed":true}))
    );
    assert!(read_marker("LEGION_DRM_PROBE broken\n", "LEGION_DRM_PROBE ").is_err());
    Ok(())
}
fn device(driver: &str, accessible: bool, vendor: &str, index: u32) -> doctor::RenderDevice {
    doctor::RenderDevice {
        node: format!("/dev/dri/renderD{index}").into(),
        pci_vendor: Some(vendor.into()),
        driver: Some(driver.into()),
        read_write_access: accessible,
        backend: "test",
        backend_status: "test",
    }
}
#[test]
fn selects_by_loaded_driver_and_access_instead_of_vendor_id() -> Result<()> {
    let devices = vec![
        device("nvidia", true, "0x1002", 128),
        device("amdgpu", false, "0x1002", 129),
        device("amdgpu", true, "0x1002", 130),
    ];
    assert_eq!(
        select(&devices, None)?,
        PathBuf::from("/dev/dri/renderD130")
    );
    assert!(select(&devices, Some(Path::new("/dev/dri/renderD128"))).is_err());
    let mut multiple = devices;
    multiple.push(device("amdgpu", true, "0x1002", 131));
    assert!(select(&multiple, None).is_err());
    assert_eq!(
        select(&multiple, Some(Path::new("/dev/dri/renderD131")))?,
        PathBuf::from("/dev/dri/renderD131")
    );
    Ok(())
}
