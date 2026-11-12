//! Validated virtiofs shares.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const MANIFEST: &str = "guest-shares.json";
pub const GUEST_ROOT: &str = "/run/legion-shares";
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
pub const LIBRARY_MANIFEST: &str = "guest-libraries.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryBinding {
    pub tag: String,
    pub source_name: String,
    pub target_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LibraryBindings {
    pub files: Vec<LibraryBinding>,
}

impl LibraryBindings {
    pub fn validate(&self, mounts: &Manifest) -> Result<()> {
        mounts.validate()?;
        ensure!(self.files.len() <= 256, "too many driver library bindings");
        let mut targets = BTreeSet::new();
        for file in &self.files {
            ensure!(
                mounts
                    .mounts
                    .iter()
                    .any(|m| m.tag == file.tag && m.readonly),
                "driver libraries require an existing read-only share"
            );
            for name in [&file.source_name, &file.target_name] {
                ensure!(
                    Path::new(name).components().count() == 1
                        && matches!(
                            Path::new(name).components().next(),
                            Some(std::path::Component::Normal(_))
                        )
                        && name
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
                        && name.contains(".so")
                        && [
                            "libnvidia-",
                            "libcuda.so",
                            "libnvcuvid.so",
                            "libGLX_nvidia.so",
                            "libEGL_nvidia.so"
                        ]
                        .iter()
                        .any(|prefix| name.starts_with(prefix)),
                    "only NVIDIA driver library basenames may be bound"
                );
            }
            ensure!(
                targets.insert(&file.target_name),
                "duplicate driver library target"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub tag: String,
    pub directory: PathBuf,
    pub readonly: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Sources {
    pub schema: u32,
    pub directories: Vec<Source>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestMount {
    pub tag: String,
    pub readonly: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub mounts: Vec<GuestMount>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Mounted {
    pub tag: String,
    pub target: PathBuf,
    pub readonly: bool,
}

impl Sources {
    /// Resolve existing host directories; never create or modify them.
    pub fn resolve(mut self) -> Result<Self> {
        self.manifest().validate()?;
        for share in &mut self.directories {
            ensure!(
                share.directory.is_absolute(),
                "share directory must be absolute: {}",
                share.directory.display()
            );
            share.directory = share
                .directory
                .canonicalize()
                .with_context(|| format!("resolve share {}", share.tag))?;
            ensure!(
                share.directory.is_dir(),
                "share {} is not a directory",
                share.tag
            );
        }
        Ok(self)
    }

    pub fn manifest(&self) -> Manifest {
        Manifest {
            schema: self.schema,
            mounts: self
                .directories
                .iter()
                .map(|s| GuestMount {
                    tag: s.tag.clone(),
                    readonly: s.readonly,
                })
                .collect(),
        }
    }
}

impl Manifest {
    pub fn verify_mounted(&self, mounted: &[Mounted]) -> Result<()> {
        self.validate()?;
        let expected: Vec<_> = self
            .mounts
            .iter()
            .map(|mount| Mounted {
                tag: mount.tag.clone(),
                target: mount.target(),
                readonly: mount.readonly,
            })
            .collect();
        ensure!(
            expected == mounted,
            "guest directory-share acknowledgement does not match requested mounts"
        );
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "unsupported directory-share schema");
        ensure!(
            self.mounts.len() <= 16,
            "at most 16 extra directory shares are supported"
        );
        let mut tags = BTreeSet::new();
        for mount in &self.mounts {
            ensure!(
                mount.tag != "proton-tool" || mount.readonly,
                "proton-tool must be shared readonly"
            );
            ensure!(
                !matches!(
                    mount.tag.as_str(),
                    "proton-compatdata" | "proton-cache" | "proton-home"
                ) || !mount.readonly,
                "Proton state shares must be writable"
            );
            let tag = mount.tag.as_bytes();
            ensure!(
                !tag.is_empty()
                    && tag.len() <= 24
                    && tag[0].is_ascii_lowercase()
                    && tag
                        .iter()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-'),
                "invalid directory-share tag: {:?}",
                mount.tag
            );
            ensure!(
                mount.tag != "usr" && mount.tag != "frames",
                "reserved directory-share tag: {}",
                mount.tag
            );
            ensure!(
                tags.insert(&mount.tag),
                "duplicate directory-share tag: {}",
                mount.tag
            );
        }
        Ok(())
    }
}

impl GuestMount {
    /// Call only after the containing manifest has passed validation.
    pub fn target(&self) -> PathBuf {
        Path::new(GUEST_ROOT).join(&self.tag)
    }
}

#[cfg(test)]
mod tests {
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
}
