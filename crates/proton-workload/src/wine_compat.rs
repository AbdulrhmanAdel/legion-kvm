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
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Overlay, PathBuf) {
        let root = tempfile::tempdir().expect("test directory");
        for (name, bytes) in [
            ("x86_64-windows/ntdll.dll", b"original".as_slice()),
            ("x86_64-unix/ntdll.so", b"unix".as_slice()),
        ] {
            let path = root.path().join("files/lib/wine").join(name);
            fs::create_dir_all(path.parent().expect("parent")).expect("directories");
            fs::write(path, bytes).expect("module");
        }
        let replacement = root.path().join("replacement.dll");
        fs::write(&replacement, b"repaired").expect("replacement");
        let overlay = Overlay {
            stock_pe_sha256: digest(&root.path().join("files/lib/wine/x86_64-windows/ntdll.dll"))
                .expect("PE digest"),
            stock_unix_sha256: digest(&root.path().join("files/lib/wine/x86_64-unix/ntdll.so"))
                .expect("Unix digest"),
            replacement_sha256: digest(&replacement).expect("replacement digest"),
            migrate_loader: true,
        };
        (root, overlay, replacement)
    }

    #[test]
    fn rejects_each_mismatched_component_before_prefix_changes() {
        for component in [
            "files/lib/wine/x86_64-windows/ntdll.dll",
            "files/lib/wine/x86_64-unix/ntdll.so",
            "replacement.dll",
        ] {
            let (root, overlay, replacement) = fixture();
            fs::write(root.path().join(component), b"different build").expect("mismatch");
            assert!(overlay.validate(root.path(), &replacement).is_err());
        }
    }

    #[test]
    fn prefix_validation_preserves_module_and_external_symlink() {
        let (root, overlay, replacement) = fixture();
        let source = root.path().join("files/lib/wine/x86_64-windows/ntdll.dll");
        let prefix = root.path().join("prefix.dll");
        std::os::unix::fs::symlink(&source, &prefix).expect("prefix link");
        let validated = overlay
            .validate(root.path(), &replacement)
            .expect("matched pair");
        validated.validate_prefix(&prefix).expect("stock prefix");
        assert_eq!(fs::read_link(&prefix).expect("preserved link"), source);
        assert_eq!(fs::read(&source).expect("preserved source"), b"original");
        fs::write(&source, b"custom module").expect("custom module");
        assert!(validated.validate_prefix(&prefix).is_err());
        assert_eq!(
            fs::read(&prefix).expect("preserved custom module"),
            b"custom module"
        );
        assert!(prefix.is_symlink());
    }

    #[test]
    fn replaces_external_prefix_link_without_writing_the_source() {
        let (root, overlay, replacement) = fixture();
        let source = root.path().join("files/lib/wine/x86_64-windows/ntdll.dll");
        let prefix = root.path().join("prefix.dll");
        std::os::unix::fs::symlink(&source, &prefix).expect("prefix link");
        let validated = overlay
            .validate(root.path(), &replacement)
            .expect("matched pair");
        assert!(validated.prepare_prefix(&prefix).expect("publish"));
        assert_eq!(fs::read(&source).expect("stock source"), b"original");
        assert_eq!(fs::read(&prefix).expect("private prefix"), b"repaired");
        assert!(!prefix.is_symlink());
        assert!(!validated.prepare_prefix(&prefix).expect("idempotent reuse"));
    }

    #[test]
    fn preserves_unknown_prefix_and_rejects_replacement_changes() {
        let (root, overlay, replacement) = fixture();
        let validated = overlay
            .validate(root.path(), &replacement)
            .expect("matched pair");
        let prefix = root.path().join("prefix.dll");
        fs::write(&prefix, b"custom module").expect("custom prefix");
        assert!(validated.prepare_prefix(&prefix).is_err());
        assert_eq!(fs::read(&prefix).expect("preserved"), b"custom module");
        fs::write(&prefix, b"original").expect("stock prefix");
        fs::write(&replacement, b"changed").expect("changed overlay");
        assert!(validated.prepare_prefix(&prefix).is_err());
        assert_eq!(fs::read(prefix).expect("preserved stock"), b"original");
    }

    #[test]
    fn staging_collision_preserves_existing_file_and_prefix() {
        let (root, overlay, replacement) = fixture();
        let validated = overlay
            .validate(root.path(), &replacement)
            .expect("matched pair");
        let prefix = root.path().join("prefix.dll");
        fs::write(&prefix, b"original").expect("stock prefix");
        let staging = root
            .path()
            .join(format!(".legion-ntdll-{}.tmp", std::process::id()));
        std::os::unix::fs::symlink(&prefix, &staging).expect("existing staging link");
        assert!(validated.prepare_prefix(&prefix).is_err());
        assert_eq!(fs::read(prefix).expect("preserved prefix"), b"original");
        assert!(staging.is_symlink());
    }
}
