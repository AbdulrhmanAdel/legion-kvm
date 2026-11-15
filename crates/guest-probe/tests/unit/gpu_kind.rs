use super::*;
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
