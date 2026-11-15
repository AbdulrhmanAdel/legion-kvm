use super::*;

#[test]
fn loader_failure_is_not_reported_as_a_gpu_identity_mismatch() {
    let info = serde_json::json!({"success":false,"exit_code":1,"signal":null,
        "stdout":"","stderr":"Found no drivers!\nvkCreateInstance failed with ERROR_INCOMPATIBLE_DRIVER\n"});
    let error = verify_enumeration("legion.gpu_vendor=1002 legion.gpu_device=7550", &info)
        .expect_err("failed Vulkan initialization")
        .to_string();
    assert!(error.contains("ERROR_INCOMPATIBLE_DRIVER"));
    assert!(error.contains("qemu.log"));
    assert!(!error.contains("selected GPU"));
}

#[test]
fn successful_loader_still_requires_the_selected_hardware_through_venus() -> Result<()> {
    let cmdline = "legion.gpu_vendor=1002 legion.gpu_device=7550";
    let mut info = serde_json::json!({"success":true,"stdout":
        "vendorID = 0x1002\ndeviceID = 0x7550\ndeviceType = PHYSICAL_DEVICE_TYPE_DISCRETE_GPU\ndriverID = DRIVER_ID_MESA_VENUS\n"});
    verify_enumeration(cmdline, &info)?;
    assert!(verify_enumeration("legion.gpu_vendor=1002 legion.gpu_device=13c0", &info).is_err());
    info["stdout"] =
        "driverID = DRIVER_ID_MESA_VENUS\ndeviceType = PHYSICAL_DEVICE_TYPE_CPU".into();
    assert!(verify_enumeration(cmdline, &info).is_err());
    Ok(())
}
#[test]
fn driver_selection_is_explicit_and_unambiguous() -> Result<()> {
    assert_eq!(
        GpuKind::from_cmdline("console=ttyS0 legion.vulkan=1")?,
        GpuKind::Venus
    );
    let venus = GpuKind::from_cmdline("legion.gpu=venus")?;
    assert_eq!(venus, GpuKind::Venus);
    assert!(venus.icd().ends_with("virtio_icd.json"));
    assert!(GpuKind::from_cmdline("legion.gpu=unknown").is_err());
    assert!(GpuKind::from_cmdline("legion.gpu=nvidia legion.gpu=amdgpu").is_err());
    Ok(())
}

#[test]
fn missing_amd_driver_cannot_silently_validate_nvidia() -> Result<()> {
    let amd = "legion.gpu_vendor=1002 legion.gpu_device=13c0";
    let nvidia = "vendorID = 0x10de\ndeviceID = 0x2705\n";
    let integrated = "vendorID = 0x1002\ndeviceID = 0x13c0\n";
    assert!(verify_host_gpu(amd, nvidia).is_err());
    verify_host_gpu(amd, integrated)?;
    assert!(verify_host_gpu(amd, &format!("{nvidia}{integrated}")).is_err());
    assert!(verify_host_gpu(amd, "").is_err());
    Ok(())
}

#[test]
fn host_gpu_contract_rejects_partial_duplicate_or_invalid_identity() -> Result<()> {
    for cmdline in [
        "legion.gpu_vendor=1002",
        "legion.gpu_vendor=invalid legion.gpu_device=13c0",
        "legion.gpu_vendor=1002 legion.gpu_vendor=10de legion.gpu_device=13c0",
    ] {
        assert!(verify_host_gpu(cmdline, "").is_err());
    }
    verify_host_gpu("legion.gpu=venus", "")?;
    Ok(())
}
