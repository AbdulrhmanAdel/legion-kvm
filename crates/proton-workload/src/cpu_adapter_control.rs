//! File control for the disposable guest adapter; contains no interception.
use anyhow::{Context, Result, ensure};
use std::{fs, io::Read, os::unix::fs::OpenOptionsExt, path::Path};

pub const LIBRARY: &str = "/usr/share/legion/liblegion_cpu_adapter.so";

pub fn create_state(path: &Path) -> Result<()> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .context("create fresh guest CPU adapter state")?;
    file.set_len(4096).context("size guest CPU adapter state")
}

pub fn read_state(path: &Path) -> Result<serde_json::Value> {
    let mut bytes = [0_u8; 176];
    fs::File::open(path)?.read_exact(&mut bytes)?;
    let mut fields = [0_u64; 22];
    for (field, chunk) in fields.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *field = u64::from_le_bytes(*chunk);
    }
    ensure!(
        fields[0] == u64::from_le_bytes(*b"LCPID001"),
        "CPU adapter did not initialize"
    );
    ensure!(fields[5] > 0, "CPU adapter did not intercept CPUID");
    Ok(
        serde_json::json!({"schema":1,"owner_linux_tgid":fields[1],"declared_windows_pid":fields[2],
        "handler":fields[3],"ready":fields[4],"queries":fields[5],"failures":fields[6],
        "delegated_signals":fields[7],"kuser_status":fields[8],"kuser_updates":fields[9],"kuser_error":fields[10],
        "readiness_producer_implemented":fields[8]==2,"nt_syscall_delivery_implemented":fields[15]!=0 && fields[16]!=0,
        "direct_nt_syscall_enabled":fields[15]!=0,"nt_redirected":fields[12],"nt_forwarded":fields[13],"nt_unrelated":fields[14],
        "nt_trap_codes":{"seccomp":fields[15]&2!=0,"syscall_user_dispatch":fields[15]&4!=0},
        "wine_dispatch_installed":fields[16]!=0,"wine_dispatch_redirected":fields[17],"wine_dispatch_forwarded":fields[18],
        "resolver_installed":fields[19]!=0,"resolver_outer":fields[20],"resolver_nested":fields[21]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn never_reuses_previous_state() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("state");
        create_state(&path)?;
        fs::write(&path, b"previous run")?;
        assert!(create_state(&path).is_err());
        assert_eq!(fs::read(&path)?, b"previous run");
        Ok(())
    }
    #[test]
    fn requires_initialized_and_exercised_adapter() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("state");
        create_state(&path)?;
        assert!(read_state(&path).is_err());
        let mut bytes = [0_u8; 176];
        bytes[..8].copy_from_slice(b"LCPID001");
        fs::write(&path, bytes)?;
        assert!(read_state(&path).is_err());
        bytes[40..48].copy_from_slice(&1_u64.to_le_bytes());
        fs::write(&path, bytes)?;
        assert_eq!(read_state(&path)?["queries"], 1);
        Ok(())
    }
}
