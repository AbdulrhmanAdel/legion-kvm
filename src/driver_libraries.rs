//! Share installed NVIDIA vendor binaries read-only; keep guest libc private.
use anyhow::{Context, Result, ensure};
use legion_shared_directories::{LibraryBinding, LibraryBindings, Source};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

fn vendor_name(name: &str) -> bool {
    [
        "libnvidia-",
        "libcuda.so",
        "libnvcuvid.so",
        "libGLX_nvidia.so",
        "libEGL_nvidia.so",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
        && name.contains(".so")
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

/// Read the loaded module, rather than the driver used to build Legion.
pub(crate) fn loaded_version() -> Result<String> {
    version_from_proc(
        &fs::read_to_string("/proc/driver/nvidia/version")
            .context("the NVIDIA backend requires a loaded NVIDIA driver")?,
    )
}

fn version_from_proc(text: &str) -> Result<String> {
    let line = text
        .lines()
        .find(|line| line.starts_with("NVRM version:"))
        .context("NVIDIA module version line is missing")?;
    line.split_whitespace()
        .find(|word| {
            let parts: Vec<_> = word.split('.').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        })
        .map(str::to_owned)
        .context("loaded NVIDIA driver version is not recognized")
}

fn matching_library(source: &Path, required: &str, loaded: &str) -> Result<()> {
    ensure!(
        source
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(&format!(".so.{loaded}"))),
        "installed {required} does not match the loaded NVIDIA driver {loaded}; check for a pending driver update/reboot"
    );
    Ok(())
}

pub fn stage(usr: &Path, shares: &mut Vec<Source>) -> Result<LibraryBindings> {
    let driver_version = loaded_version()?;
    let ldconfig = ["/sbin/ldconfig", "/usr/sbin/ldconfig"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .unwrap_or("ldconfig");
    let reply = Command::new(ldconfig)
        .arg("-p")
        .output()
        .context("locate installed NVIDIA libraries using ldconfig")?;
    ensure!(
        reply.status.success(),
        "ldconfig could not enumerate host libraries"
    );
    let mut aliases = BTreeMap::new();
    for line in String::from_utf8(reply.stdout)?.lines() {
        let Some((left, right)) = line.split_once("=>") else {
            continue;
        };
        let Some(alias) = left.split_whitespace().next() else {
            continue;
        };
        if !vendor_name(alias) {
            continue;
        }
        let source = Path::new(right.trim()).canonicalize()?;
        let mut header = [0_u8; 5];
        fs::File::open(&source)?.read_exact(&mut header)?;
        if header != [0x7f, b'E', b'L', b'F', 2] {
            continue;
        }
        let name = source
            .file_name()
            .and_then(|v| v.to_str())
            .context("invalid driver filename")?;
        if !vendor_name(name) {
            continue;
        }
        aliases
            .entry(alias.to_owned())
            .or_insert_with(|| source.clone());
        aliases.entry(name.to_owned()).or_insert(source);
    }
    for required in ["libGLX_nvidia.so.0", "libEGL_nvidia.so.0"] {
        let source = aliases
            .get(required)
            .with_context(|| format!("installed driver lacks {required}"))?;
        matching_library(source, required, &driver_version)?;
    }
    ensure!(
        aliases.len() <= 256,
        "driver library binding limit exceeded"
    );
    let parents: BTreeSet<PathBuf> = aliases
        .values()
        .filter_map(|p| p.parent().map(Path::to_owned))
        .collect();
    let mut tags = BTreeMap::new();
    for (index, directory) in parents.into_iter().enumerate() {
        let tag = format!("nvidia-driver-{index}");
        shares.push(Source {
            tag: tag.clone(),
            directory: directory.clone(),
            readonly: true,
        });
        tags.insert(directory, tag);
    }
    let mut files = Vec::new();
    for (target_name, source) in aliases {
        let target = usr.join("lib").join(&target_name);
        if target.is_file() && target.metadata()?.len() > 0 {
            // Small guest-compatible platform helpers can remain embedded.
            continue;
        }
        if target.is_symlink() || target.is_file() {
            fs::remove_file(&target)?;
        }
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        let parent = source.parent().context("driver library has no directory")?;
        files.push(LibraryBinding {
            tag: tags
                .get(parent)
                .context("driver share not registered")?
                .clone(),
            source_name: source
                .file_name()
                .context("driver basename missing")?
                .to_str()
                .context("invalid driver basename")?
                .to_owned(),
            target_name,
        });
    }
    Ok(LibraryBindings { files })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
