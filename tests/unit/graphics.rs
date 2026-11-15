use super::*;
fn device(driver: &str, node: &str) -> RenderDevice {
    RenderDevice {
        node: node.into(),
        pci_vendor: None,
        driver: Some(driver.into()),
        read_write_access: true,
        backend: "test",
        backend_status: "test",
    }
}
#[test]
fn mixed_default_is_preserved_and_explicit_selection_never_falls_back() -> Result<()> {
    let devices = [
        device("amdgpu", "/dev/dri/renderD129"),
        device("nvidia", "/dev/dri/renderD128"),
    ];
    assert_eq!(
        select(&devices, None)?,
        Graphics {
            node: "/dev/dri/renderD128".into(),
            driver: "nvidia".into()
        }
    );
    assert_eq!(
        select(&devices, Some(Path::new("/dev/dri/renderD129")))?,
        Graphics {
            node: "/dev/dri/renderD129".into(),
            driver: "amdgpu".into()
        }
    );
    assert!(select(&devices, Some(Path::new("/missing"))).is_err());
    assert_eq!(select(&devices[..1], None)?.driver, "amdgpu");
    Ok(())
}
