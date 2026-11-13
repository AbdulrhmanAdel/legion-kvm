//! Opt-in, build-matched PE ntdll overlay for disposable guest experiments.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_MODULE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Overlay {
    pub stock_pe_sha256: String,
    pub stock_unix_sha256: String,
    pub replacement_sha256: String,
    pub migrate_loader: bool,
}

pub(crate) struct Validated {
    replacement: PathBuf,
    original_hash: String,
    replacement_hash: String,
}

fn module_bytes(path: &Path) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "Wine module must be a regular file"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_MODULE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_MODULE_BYTES,
        "invalid Wine module size"
    );
    Ok(bytes)
}

fn digest(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(module_bytes(path)?)))
}

impl Overlay {
    pub fn validate(&self, proton: &Path, replacement: &Path) -> Result<Validated> {
        for hash in [
            &self.stock_pe_sha256,
            &self.stock_unix_sha256,
            &self.replacement_sha256,
        ] {
            ensure!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v)),
                "Wine overlay requires lowercase SHA-256 digests"
            );
        }
        ensure!(
            digest(&proton.join("files/lib/wine/x86_64-windows/ntdll.dll"))?
                == self.stock_pe_sha256,
            "Wine overlay does not match selected Proton's Windows ntdll"
        );
        ensure!(
            digest(&proton.join("files/lib/wine/x86_64-unix/ntdll.so"))? == self.stock_unix_sha256,
            "Wine overlay does not match selected Proton's Unix ntdll"
        );
        ensure!(
            digest(replacement)? == self.replacement_sha256,
            "Wine overlay replacement digest mismatch"
        );
        Ok(Validated {
            replacement: replacement.to_owned(),
            original_hash: self.stock_pe_sha256.clone(),
            replacement_hash: self.replacement_sha256.clone(),
        })
    }
}

impl Validated {
    pub fn validate_prefix(&self, module: &Path) -> Result<()> {
        let current = digest(module)?;
        ensure!(
            current == self.original_hash || current == self.replacement_hash,
            "refusing to overlay an unknown prefix ntdll"
        );
        Ok(())
    }
    pub fn replacement(&self) -> &Path {
        &self.replacement
    }

    /// Update only a known stock copy in this run's explicit prefix.
    pub fn prepare_prefix(&self, module: &Path) -> Result<bool> {
        let current = digest(module)?;
        if current == self.replacement_hash {
            return Ok(false);
        }
        ensure!(
            current == self.original_hash,
            "refusing to replace an unknown prefix ntdll"
        );
        let replacement = module_bytes(&self.replacement)?;
        ensure!(
            format!("{:x}", Sha256::digest(&replacement)) == self.replacement_hash,
            "Wine overlay changed after validation"
        );
        let parent = module.parent().context("missing prefix module parent")?;
        let staging = parent.join(format!(".legion-ntdll-{}.tmp", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&staging)?;
        let result: Result<()> = (|| {
            file.write_all(&replacement)?;
            file.flush()?;
            drop(file);
            fs::rename(&staging, module).context("publish private prefix ntdll")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(staging);
        }
        result?;
        Ok(true)
    }
}

#[cfg(test)]
#[path = "../tests/unit/wine_compat.rs"]
mod tests;
