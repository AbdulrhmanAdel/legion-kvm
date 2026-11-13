use super::*;

#[test]
fn shares_driver_libraries_without_replacing_portable_platform_helpers() {
    for name in [
        "libnvidia-egl-gbm.so.1.1.3",
        "libnvidia-egl-wayland.so.1",
        "libnvidia-egl-wayland2.so.1",
        "libnvidia-egl-xcb.so.1",
        "libnvidia-egl-xlib.so.1",
        "../libGLX_nvidia.so.0",
        "libc.so.6",
    ] {
        assert!(!vendor_name(name), "{name}");
    }
    for name in [
        "libGLX_nvidia.so.0",
        "libEGL_nvidia.so.615.78.08",
        "libnvidia-glcore.so.615.78.08",
        "libnvidia-glvkspirv.so.615.78.08",
        "libcuda.so.1",
    ] {
        assert!(vendor_name(name), "{name}");
    }
}

#[test]
fn detects_loaded_releases_independently_of_packaged_version() -> Result<()> {
    for (text, version) in [
        (
            "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64 615.78.08 Release Build\nGCC version: 16.2.1",
            "615.78.08",
        ),
        (
            "NVRM version: NVIDIA UNIX x86_64 Kernel Module 580.178.04 Tue Jul 7",
            "580.178.04",
        ),
        (
            "NVRM version: NVIDIA UNIX x86_64 Kernel Module 535.129.03",
            "535.129.03",
        ),
    ] {
        assert_eq!(version_from_proc(text)?, version);
        matching_library(
            &PathBuf::from(format!("/driver/libGLX_nvidia.so.{version}")),
            "libGLX_nvidia.so.0",
            version,
        )?;
    }
    assert!(version_from_proc("NVRM version: unknown\nGCC version: 16.2.1").is_err());
    assert!(version_from_proc("GCC version: 16.2.1").is_err());
    assert!(
        matching_library(
            Path::new("/driver/libGLX_nvidia.so.615.78.08"),
            "libGLX_nvidia.so.0",
            "580.178.04"
        )
        .is_err()
    );
    Ok(())
}
