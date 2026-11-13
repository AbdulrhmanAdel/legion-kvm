//! Versioned, compressed runtime appended to the static executable.
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Component, Path, PathBuf},
    process::Command,
};

const MAGIC: &[u8; 16] = b"LEGION_BUNDLE_01";
const FOOTER: u64 = 56;
const MAX_PAYLOAD: u64 = 512 * 1024 * 1024;
const MAX_EXPANDED: u64 = 2 * 1024 * 1024 * 1024;

pub(crate) fn data_root(variable: &str, fallback: &str) -> Result<PathBuf> {
    let base = match std::env::var_os(variable) {
        Some(value) => PathBuf::from(value),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is missing")?).join(fallback),
    };
    ensure!(base.is_absolute(), "{variable} must be absolute");
    private_directory(&base.join("legion"))
}

fn private_directory(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir()
            && !meta.file_type().is_symlink()
            && meta.uid() == rustix::process::getuid().as_raw()
            && meta.mode() & 0o077 == 0,
        "Legion cache must be a private directory owned by your user: {}",
        path.display()
    );
    path.canonicalize().map_err(Into::into)
}

fn relative(bytes: &[u8]) -> Result<PathBuf> {
    let text = std::str::from_utf8(bytes)?;
    let path = Path::new(text);
    ensure!(
        !text.is_empty()
            && !text.contains('\0')
            && path.components().all(|p| matches!(p, Component::Normal(_))),
        "invalid runtime archive path"
    );
    Ok(path.to_owned())
}

fn read_bytes(input: &mut impl Read, size: u64, maximum: u64) -> Result<Vec<u8>> {
    ensure!(size <= maximum, "runtime entry exceeds size limit");
    let mut bytes = vec![0; usize::try_from(size)?];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn unpack(input: &mut impl Read, root: &Path) -> Result<()> {
    let mut total = 0_u64;
    for _ in 0..100_000 {
        let mut kind = [0];
        input.read_exact(&mut kind)?;
        if kind[0] == 255 {
            let mut extra = [0];
            ensure!(
                input.read(&mut extra)? == 0,
                "unexpected data after runtime archive"
            );
            return Ok(());
        }
        let mut header = [0; 16];
        input.read_exact(&mut header)?;
        let name_length = u32::from_le_bytes(header[..4].try_into()?);
        let length = u64::from_le_bytes(header[4..12].try_into()?);
        let mode = u32::from_le_bytes(header[12..].try_into()?);
        let name = relative(&read_bytes(input, u64::from(name_length), 4096)?)?;
        total = total.checked_add(length).context("runtime size overflow")?;
        ensure!(total <= MAX_EXPANDED, "runtime archive is too large");
        let destination = root.join(&name);
        let parent = destination
            .parent()
            .context("runtime entry has no parent")?;
        let mut current = root.to_owned();
        for part in parent.strip_prefix(root)?.components() {
            current.push(part);
            let meta = fs::symlink_metadata(&current)?;
            ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "runtime entry traverses a symlink"
            );
        }
        match kind[0] {
            0 => {
                ensure!(length <= 128 * 1024 * 1024, "runtime file is too large");
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination)?;
                let copied = std::io::copy(&mut Read::by_ref(input).take(length), &mut output)?;
                ensure!(copied == length, "truncated runtime file");
                output.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
            }
            1 => {
                ensure!(length == 0, "directory contains data");
                fs::create_dir(&destination)?;
            }
            2 => {
                let target = read_bytes(input, length, 4096)?;
                let target = std::str::from_utf8(&target)?;
                ensure!(!target.contains('\0'), "invalid runtime symlink");
                // Absolute /usr links describe the guest filesystem.
                std::os::unix::fs::symlink(target, &destination)?;
            }
            3 => {
                let source = root.join(relative(&read_bytes(input, length, 4096)?)?);
                ensure!(
                    source.is_file()
                        && !source.is_symlink()
                        && source.canonicalize()?.starts_with(root),
                    "invalid runtime hardlink"
                );
                fs::hard_link(source, destination)?;
            }
            _ => bail!("unsupported runtime archive entry"),
        }
    }
    bail!("too many runtime archive entries")
}

fn verify(root: &Path) -> Result<()> {
    let checks = fs::read_to_string(root.join("SHA256SUMS"))?;
    ensure!(
        checks.len() <= 4 * 1024 * 1024,
        "runtime checksum manifest is too large"
    );
    for line in checks.lines() {
        let (expected, name) = line.split_once("  ").context("invalid checksum record")?;
        let path = root.join(relative(name.as_bytes())?);
        ensure!(
            fs::symlink_metadata(&path)?.is_file() && path.canonicalize()?.starts_with(root),
            "invalid cached runtime file"
        );
        let mut file = File::open(path)?;
        let mut hash = Sha256::new();
        std::io::copy(&mut file, &mut hash)?;
        ensure!(
            format!("{:x}", hash.finalize()) == expected,
            "cached runtime checksum failed: {name}"
        );
    }
    ensure!(
        root.join("runtime.json").is_file() && root.join("bin/legion").is_file(),
        "runtime is incomplete"
    );
    Ok(())
}

pub(crate) fn enter(args: &[OsString]) -> Result<()> {
    let mut file = File::open(std::env::current_exe()?)?;
    let size = file.metadata()?.len();
    if size < FOOTER {
        return Ok(());
    }
    file.seek(SeekFrom::End(-i64::try_from(FOOTER)?))?;
    let mut footer = [0; 56];
    file.read_exact(&mut footer)?;
    if &footer[..16] != MAGIC {
        return Ok(());
    }
    ensure!(
        !rustix::process::getuid().is_root(),
        "run Legion as your desktop user"
    );
    let length = u64::from_le_bytes(footer[16..24].try_into()?);
    ensure!(
        length > 0 && length <= MAX_PAYLOAD,
        "invalid embedded runtime length"
    );
    let start = size
        .checked_sub(FOOTER)
        .and_then(|s| s.checked_sub(length))
        .context("truncated embedded runtime")?;
    let cache = data_root("XDG_CACHE_HOME", ".cache")?;
    let digest = footer[24..]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let root = cache.join(format!("runtime-{digest}"));
    let lock = rustix::fs::open(
        cache.join("unpack.lock"),
        rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
    if root.exists() {
        private_directory(&root)?;
        if verify(&root).is_err() {
            fs::remove_dir_all(&root)?;
        }
    }
    if !root.exists() {
        eprintln!("legion: Preparing the bundled runtime…");
        file.seek(SeekFrom::Start(start))?;
        let mut hash = Sha256::new();
        std::io::copy(&mut Read::by_ref(&mut file).take(length), &mut hash)?;
        ensure!(
            hash.finalize().as_slice() == &footer[24..],
            "embedded runtime checksum failed"
        );
        file.seek(SeekFrom::Start(start))?;
        let temporary = tempfile::Builder::new()
            .prefix("unpack-")
            .tempdir_in(&cache)?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let mut decoder =
            ruzstd::decoding::StreamingDecoder::new(Read::by_ref(&mut file).take(length))?;
        unpack(&mut decoder, temporary.path())?;
        verify(temporary.path())?;
        fs::rename(temporary.path(), &root)?;
    }
    drop(lock);
    Err(Command::new(root.join("bin/legion")).args(args).exec()).context("start bundled Legion")
}

#[cfg(test)]
#[path = "../tests/unit/packed.rs"]
mod tests;
