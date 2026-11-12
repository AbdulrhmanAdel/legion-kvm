//! Keep shared guest RAM alive in the launcher and expose an aligned mapping through QEMU's standard file backend.
use anyhow::{Result, ensure};
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, ftruncate, memfd_create};
use serde::Serialize;
use std::{
    fs,
    os::fd::{AsRawFd, OwnedFd},
};

#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Legacy,
    #[default]
    Aligned,
}

#[derive(Clone, Debug, Serialize)]
pub struct Info {
    pub mode: Mode,
    pub size_mib: u64,
    pub requested_alignment_bytes: Option<u64>,
}

pub struct Ram {
    // QEMU opens this through the parent's proc-fd path.
    _backing: Option<OwnedFd>,
    pub info: Info,
    pub argument: String,
}

fn alignment(hint: Option<&str>) -> u64 {
    hint.and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| n.is_power_of_two() && (4096..=1 << 30).contains(n))
        .unwrap_or(4096)
}

impl Ram {
    pub fn new(mode: Mode, size_mib: u64) -> Result<Self> {
        let bytes = size_mib
            .checked_mul(1024 * 1024)
            .filter(|&n| n > 0)
            .ok_or_else(|| anyhow::anyhow!("invalid guest RAM size"))?;
        if matches!(mode, Mode::Legacy) {
            return Ok(Self {
                _backing: None,
                info: Info {
                    mode,
                    size_mib,
                    requested_alignment_bytes: None,
                },
                argument: format!("memory-backend-memfd,id=ram,size={size_mib}M,share=on"),
            });
        }
        let hint = fs::read_to_string("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size").ok();
        let alignment = alignment(hint.as_deref());
        let fd = memfd_create(
            c"legion-ram",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )?;
        ftruncate(&fd, bytes)?;
        fcntl_add_seals(&fd, SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL)?;
        let path = format!("/proc/{}/fd/{}", std::process::id(), fd.as_raw_fd());
        // The generated path has only fixed text and decimal IDs, so no QEMU key/value escaping or inherited descriptor mutation is necessary.
        ensure!(!path.contains(','), "invalid generated RAM descriptor path");
        Ok(Self {
            _backing: Some(fd),
            info: Info {
                mode,
                size_mib,
                requested_alignment_bytes: Some(alignment),
            },
            argument: format!(
                "memory-backend-file,id=ram,mem-path={path},size={size_mib}M,share=on,align={alignment}"
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment_hint_requires_a_bounded_power_of_two() {
        assert_eq!(alignment(Some("2097152\n")), 2097152);
        for hint in [
            None,
            Some("0"),
            Some("4097"),
            Some("1099511627776"),
            Some("invalid"),
        ] {
            assert_eq!(alignment(hint), 4096);
        }
    }

    #[test]
    #[cfg(not(miri))]
    fn aligned_ram_keeps_a_shared_size_sealed_descriptor_without_preallocation() {
        let ram = Ram::new(Mode::Aligned, 4).expect("owned RAM");
        let fd = ram._backing.as_ref().expect("retained descriptor");
        let path = format!("/proc/{}/fd/{}", std::process::id(), fd.as_raw_fd());
        let peer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("same-user QEMU-style open");
        assert_eq!(
            peer.metadata().expect("RAM metadata").len(),
            4 * 1024 * 1024
        );
        assert!(ftruncate(fd, 2 * 1024 * 1024).is_err());
        assert!(ram.argument.contains("memory-backend-file"));
        assert!(ram.argument.contains("share=on,align="));
    }
}
