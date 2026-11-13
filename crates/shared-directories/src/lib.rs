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
#[path = "../tests/unit/lib.rs"]
mod tests;
