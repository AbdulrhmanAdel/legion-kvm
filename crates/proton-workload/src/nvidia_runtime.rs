//! Restore the driver/Proton NVIDIA bridge in a private guest prefix.
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub(crate) struct NvidiaRuntime {
    pub driver_directory: PathBuf,
    proton_directory: PathBuf,
}
impl NvidiaRuntime {
    pub fn discover(usr: &Path, proton: &Path) -> Result<Option<Self>> {
        for relative in [
            "lib/nvidia/wine",
            "lib64/nvidia/wine",
            "lib/x86_64-linux-gnu/nvidia/wine",
        ] {
            let directory = usr.join(relative);
            let count = ["nvngx.dll", "_nvngx.dll"]
                .iter()
                .filter(|name| directory.join(name).is_file())
                .count();
            if count == 0 {
                continue;
            }
            ensure!(
                count == 2,
                "incomplete NVIDIA NGX driver bridge in {}",
                directory.display()
            );
            return Ok(Some(Self {
                driver_directory: directory,
                proton_directory: proton.to_owned(),
            }));
        }
        Ok(None)
    }
    fn proton_dll(&self, name: &str, bits32: bool) -> Option<PathBuf> {
        let arch = if bits32 {
            "i386-windows"
        } else {
            "x86_64-windows"
        };
        [
            format!("files/lib/wine/nvapi/{arch}/{name}"),
            format!("files/lib64/wine/nvapi/{name}"),
            format!("files/lib/wine/nvapi/{name}"),
        ]
        .into_iter()
        .map(|relative| self.proton_directory.join(relative))
        .find(|path| path.is_file())
    }
    /// Run after Proton's prefix setup.
    pub fn prepare_prefix(&self, prefix: &Path) -> Result<Vec<PathBuf>> {
        let system32 = prefix.join("drive_c/windows/system32");
        ensure!(
            system32.is_dir(),
            "NVIDIA setup requires an initialized private prefix"
        );
        let mut sources = vec![
            (
                self.driver_directory.join("nvngx.dll"),
                system32.join("nvngx.dll"),
            ),
            (
                self.driver_directory.join("_nvngx.dll"),
                system32.join("_nvngx.dll"),
            ),
            (
                self.proton_dll("nvapi64.dll", false)
                    .context("selected Proton lacks nvapi64.dll")?,
                system32.join("nvapi64.dll"),
            ),
        ];
        if let Some(source) = self.proton_dll("nvofapi64.dll", false) {
            sources.push((source, system32.join("nvofapi64.dll")));
        }
        if let Some(source) = self.proton_dll("nvapi.dll", true) {
            let destination = prefix.join("drive_c/windows/syswow64/nvapi.dll");
            if destination.parent().is_some_and(Path::is_dir) {
                sources.push((source, destination));
            }
        }
        let mut prepared = Vec::new();
        for (source, destination) in sources {
            ensure!(
                fs::metadata(&source)?.len() <= 64 * 1024 * 1024,
                "unexpected NVIDIA bridge size"
            );
            let bytes = fs::read(&source)?;
            // Rename a newly created temporary file rather than following an existing prefix DLL symlink back into an installed Proton tree.
            if destination.is_symlink()
                || fs::read(&destination).ok().as_deref() != Some(bytes.as_slice())
            {
                let temporary =
                    destination.with_extension(format!("legion-{}.tmp", std::process::id()));
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)?;
                let result = (|| -> Result<()> {
                    file.write_all(&bytes)?;
                    fs::rename(&temporary, &destination)?;
                    Ok(())
                })();
                if result.is_err() {
                    let _ = fs::remove_file(&temporary);
                }
                result.with_context(|| {
                    format!("prepare private NVIDIA DLL {}", destination.display())
                })?;
            }
            prepared.push(destination);
        }
        Ok(prepared)
    }
}

/// Add the transient overrides normally produced by setup_prefix, which runinprefix skips.
pub(crate) fn overrides(base: &str) -> String {
    let mut result = base.to_owned();
    for name in ["nvapi64", "nvapi", "nvofapi64", "nvngx", "_nvngx"] {
        let explicit = base
            .split(';')
            .filter_map(|entry| entry.split_once('='))
            .any(|(names, _)| {
                names
                    .split(',')
                    .any(|dll| dll.trim().eq_ignore_ascii_case(name))
            });
        if !explicit {
            if !result.is_empty() {
                result.push(';');
            }
            result.push_str(name);
            result.push_str("=n");
        }
    }
    result
}

#[cfg(test)]
mod tests {
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
}
