//! Resolve Wine prefixes and Proton compatibility directories.
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

pub(crate) struct Paths {
    pub(crate) prefix: PathBuf,
    pub(crate) compatdata: PathBuf,
}

pub(crate) fn resolve(requested: &Path, state: &Path) -> Result<Paths> {
    fs::create_dir_all(requested)?;
    let requested = requested.canonicalize()?;
    let nested = requested.join("pfx");
    if nested.join("drive_c").is_dir() {
        return Ok(Paths {
            prefix: nested.canonicalize()?,
            compatdata: requested,
        });
    }
    if let Some(parent) = requested.parent()
        && parent
            .join("pfx")
            .canonicalize()
            .is_ok_and(|path| path == requested)
        && ["version", "tracked_files", "config_info"]
            .iter()
            .any(|name| parent.join(name).is_file())
    {
        return Ok(Paths {
            prefix: requested.clone(),
            compatdata: parent.to_owned(),
        });
    }
    let key = format!(
        "{:x}",
        Sha256::digest(requested.as_os_str().as_encoded_bytes())
    );
    let compatdata = state.join("prefixes").join(key);
    fs::create_dir_all(&compatdata)?;
    let link = compatdata.join("pfx");
    if !link.exists() && !link.is_symlink() {
        symlink(&requested, &link)?;
    }
    ensure!(
        fs::read_link(&link).context("read prefix link")? == requested,
        "prefix link does not match the selected directory"
    );
    // Proton can update an existing Wine prefix before creating its file ledger.
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(compatdata.join("tracked_files"))
    {
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    Ok(Paths {
        prefix: requested,
        compatdata: compatdata.canonicalize()?,
    })
}

#[cfg(test)]
#[path = "../tests/unit/prefix.rs"]
mod tests;
