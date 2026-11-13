use super::*;
#[test]
fn other_gpu_runtime_is_untouched_and_partial_driver_install_is_reported() -> Result<()> {
    let root = tempfile::tempdir()?;
    assert!(NvidiaRuntime::discover(root.path(), root.path())?.is_none());
    fs::create_dir_all(root.path().join("lib/nvidia/wine"))?;
    fs::write(root.path().join("lib/nvidia/wine/nvngx.dll"), b"fixture")?;
    assert!(NvidiaRuntime::discover(root.path(), root.path()).is_err());
    Ok(())
}
#[test]
fn selected_proton_and_driver_refresh_private_prefix_without_following_target_symlinks()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let usr = root.path().join("usr");
    let proton = root.path().join("proton");
    let prefix = root.path().join("pfx");
    let driver = usr.join("lib/nvidia/wine");
    fs::create_dir_all(&driver)?;
    for name in ["nvngx.dll", "_nvngx.dll"] {
        fs::write(driver.join(name), b"driver bridge")?;
    }
    let native = proton.join("files/lib/wine/nvapi/x86_64-windows");
    fs::create_dir_all(&native)?;
    for name in ["nvapi64.dll", "nvofapi64.dll"] {
        fs::write(native.join(name), b"selected Proton")?;
    }
    let system32 = prefix.join("drive_c/windows/system32");
    fs::create_dir_all(&system32)?;
    let installed = root.path().join("installed.dll");
    fs::write(&installed, b"driver bridge")?;
    std::os::unix::fs::symlink(&installed, system32.join("nvngx.dll"))?;
    let runtime = NvidiaRuntime::discover(&usr, &proton)?.expect("NVIDIA bridge");
    assert_eq!(runtime.prepare_prefix(&prefix)?.len(), 4);
    assert_eq!(fs::read(&installed)?, b"driver bridge");
    assert!(
        !system32.join("nvngx.dll").is_symlink(),
        "even identical installed DLLs must become private files before chown"
    );
    assert_eq!(fs::read(system32.join("nvngx.dll"))?, b"driver bridge");
    assert_eq!(runtime.prepare_prefix(&prefix)?.len(), 4);
    Ok(())
}
#[test]
fn native_overrides_preserve_explicit_disable_and_unrelated_runtime_overrides() {
    let base = "winmm=n,b;lsteamclient=d;nvapi,nvapi64=d;_NVNGX=b";
    assert_eq!(overrides(base), format!("{base};nvofapi64=n;nvngx=n"));
}
