use super::*;

#[test]
fn driver_bindings_require_readonly_shares_and_vendor_basenames() {
    let mut mounts = Manifest {
        schema: 1,
        mounts: vec![GuestMount {
            tag: "nvidia-driver-0".into(),
            readonly: true,
        }],
    };
    let binding = LibraryBinding {
        tag: "nvidia-driver-0".into(),
        source_name: "libGLX_nvidia.so.615.78.08".into(),
        target_name: "libGLX_nvidia.so.0".into(),
    };
    let mut libraries = LibraryBindings {
        files: vec![binding.clone()],
    };
    libraries
        .validate(&mounts)
        .expect("read-only NVIDIA binding");
    mounts.mounts[0].readonly = false;
    assert!(libraries.validate(&mounts).is_err());
    mounts.mounts[0].readonly = true;
    for name in [
        "../libGLX_nvidia.so.0",
        "/libGLX_nvidia.so.0",
        "libc.so.6",
        "libnvidia-test.so\n",
    ] {
        libraries.files[0].target_name = name.into();
        assert!(libraries.validate(&mounts).is_err(), "{name}");
    }
    libraries.files = vec![binding.clone(), binding];
    assert!(libraries.validate(&mounts).is_err());
}

#[test]
fn resolves_spaces_and_symlinks_without_changing_sources() {
    let root = tempfile::tempdir().expect("fixture directory");
    let actual = root.path().join("selected Proton");
    std::fs::create_dir(&actual).expect("tool directory");
    std::fs::write(actual.join("sentinel"), b"unchanged").expect("sentinel");
    let alias = root.path().join("tool-alias");
    std::os::unix::fs::symlink(&actual, &alias).expect("tool alias");
    let sources = Sources {
        schema: 1,
        directories: vec![Source {
            tag: "proton-tool".into(),
            directory: alias,
            readonly: true,
        }],
    }
    .resolve()
    .expect("resolve shares");
    assert_eq!(sources.directories[0].directory, actual);
    assert_eq!(
        sources.manifest().mounts[0].target(),
        Path::new(GUEST_ROOT).join("proton-tool")
    );
    assert_eq!(
        std::fs::read(actual.join("sentinel")).expect("sentinel"),
        b"unchanged"
    );
}

#[test]
fn rejects_duplicate_reserved_and_escaping_tags() {
    for tags in [
        vec!["usr"],
        vec!["frames"],
        vec!["../prefix"],
        vec!["/prefix"],
        vec!["bad,option"],
        vec![""],
        vec!["same", "same"],
    ] {
        let manifest = Manifest {
            schema: 1,
            mounts: tags
                .into_iter()
                .map(|tag| GuestMount {
                    tag: tag.into(),
                    readonly: false,
                })
                .collect(),
        };
        assert!(manifest.validate().is_err());
    }
}

#[test]
fn rejects_missing_sources_without_creating_them() {
    let root = tempfile::tempdir().expect("fixture directory");
    let missing = root.path().join("missing");
    assert!(
        Sources {
            schema: 1,
            directories: vec![Source {
                tag: "prefix".into(),
                directory: missing.clone(),
                readonly: false,
            }]
        }
        .resolve()
        .is_err()
    );
    assert!(!missing.exists());
}

#[test]
fn enforces_tool_and_state_permissions_before_source_resolution() {
    for (tag, readonly) in [
        ("proton-tool", false),
        ("proton-compatdata", true),
        ("proton-cache", true),
        ("proton-home", true),
    ] {
        let manifest = Manifest {
            schema: 1,
            mounts: vec![GuestMount {
                tag: tag.into(),
                readonly,
            }],
        };
        assert!(manifest.validate().is_err());
    }
    let manifest = Manifest {
        schema: 1,
        mounts: vec![GuestMount {
            tag: "proton-tool".into(),
            readonly: true,
        }],
    };
    assert!(manifest.validate().is_ok());
}

#[test]
fn rejects_missing_or_incorrect_guest_mount_acknowledgements() {
    let manifest = Manifest {
        schema: 1,
        mounts: vec![GuestMount {
            tag: "proton-tool".into(),
            readonly: true,
        }],
    };
    let mut mounted = vec![Mounted {
        tag: "proton-tool".into(),
        target: Path::new(GUEST_ROOT).join("proton-tool"),
        readonly: true,
    }];
    assert!(manifest.verify_mounted(&mounted).is_ok());
    assert!(manifest.verify_mounted(&[]).is_err());
    mounted[0].readonly = false;
    assert!(manifest.verify_mounted(&mounted).is_err());
    mounted[0].readonly = true;
    mounted[0].target = "/somewhere-else".into();
    assert!(manifest.verify_mounted(&mounted).is_err());
}
