use super::*;
#[test]
fn driver_selection_is_explicit_and_unambiguous() -> Result<()> {
    assert_eq!(
        GpuKind::from_cmdline("console=ttyS0 legion.vulkan=1")?,
        GpuKind::Nvidia
    );
    let amd = GpuKind::from_cmdline("console=ttyS0 legion.gpu=amdgpu")?;
    assert_eq!(amd.vendor_id(), "0x1002");
    assert!(amd.icd().ends_with("radeon_icd.json"));
    assert!(GpuKind::from_cmdline("legion.gpu=unknown").is_err());
    assert!(GpuKind::from_cmdline("legion.gpu=nvidia legion.gpu=amdgpu").is_err());
    Ok(())
}
