//! Read-only KVM system queries.
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;

use serde::Serialize;

const API_VERSION: libc::Ioctl = 0xae00;
const CHECK_EXTENSION: libc::Ioctl = 0xae03;

#[derive(Clone, Debug, Serialize)]
pub struct Capabilities {
    pub api_version: u32,
    pub user_memory: bool,
    pub synchronous_mmu: bool,
    pub readonly_memory: bool,
    pub irqchip: bool,
    pub extended_cpuid: bool,
    pub memory_fault_info: bool,
    pub memory_slots: Option<u32>,
    pub recommended_vcpus: Option<u32>,
    pub maximum_vcpus: Option<u32>,
}

impl Capabilities {
    /// Kernel-side preconditions only, not GPU-driver or mapping qualification.
    pub fn fixed_backing_prerequisites_present(&self) -> bool {
        self.api_version == 12 && self.user_memory && self.synchronous_mmu
    }

    /// Explain incompatibilities with the current x86 q35 CPU probe.
    pub fn cpu_probe_issues(&self, vcpus: u32) -> Vec<String> {
        let mut issues = Vec::new();
        if self.api_version != 12 {
            issues.push(format!(
                "unsupported KVM API version {} (expected 12)",
                self.api_version
            ));
        }
        for (present, name) in [
            (self.user_memory, "user memory"),
            (self.irqchip, "in-kernel irqchip"),
            (self.extended_cpuid, "extended CPUID"),
        ] {
            if !present {
                issues.push(format!("KVM does not report {name} support"));
            }
        }
        // KVM API's documented fallback for older hosts without these caps.
        let maximum = self.maximum_vcpus.or(self.recommended_vcpus).unwrap_or(4);
        if vcpus == 0 || vcpus > maximum {
            issues.push(format!(
                "requested {vcpus} vCPUs exceeds or violates KVM limit {maximum}"
            ));
        }
        issues
    }
}

/// Query the current Linux x86-64 host through its KVM device. # Errors Returns permission/open/ioctl errors, or `Unsupported` on other targets.
pub fn inspect(path: &Path) -> io::Result<Capabilities> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "KVM discovery requires Linux x86-64",
        ));
    }
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let fd = file.as_fd();
    collect(|request, argument| query(fd, request, argument))
}

fn collect(
    mut query: impl FnMut(libc::Ioctl, libc::c_ulong) -> io::Result<u32>,
) -> io::Result<Capabilities> {
    let api_version = query(API_VERSION, 0)?;
    // Do not issue newer capability requests on an unknown API.
    if api_version != 12 {
        return Ok(Capabilities {
            api_version,
            user_memory: false,
            synchronous_mmu: false,
            readonly_memory: false,
            irqchip: false,
            extended_cpuid: false,
            memory_fault_info: false,
            memory_slots: None,
            recommended_vcpus: None,
            maximum_vcpus: None,
        });
    }
    let mut capability = |number| query(CHECK_EXTENSION, number);
    let nonzero = |value| (value != 0).then_some(value);
    Ok(Capabilities {
        api_version,
        user_memory: capability(3)? != 0,
        synchronous_mmu: capability(16)? != 0,
        readonly_memory: capability(81)? != 0,
        irqchip: capability(0)? != 0,
        extended_cpuid: capability(7)? != 0,
        memory_fault_info: capability(232)? != 0,
        memory_slots: nonzero(capability(10)?),
        recommended_vcpus: nonzero(capability(9)?),
        maximum_vcpus: nonzero(capability(66)?),
    })
}

fn query(fd: BorrowedFd<'_>, request: libc::Ioctl, argument: libc::c_ulong) -> io::Result<u32> {
    // SAFETY: only the private GET_API_VERSION and CHECK_EXTENSION requests reach here.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), request, argument) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    u32::try_from(result)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative KVM capability"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(sync_mmu: u32, max: u32, recommended: u32) -> Capabilities {
        collect(|request, cap| {
            Ok(if request == API_VERSION {
                12
            } else {
                match cap {
                    16 => sync_mmu,
                    66 => max,
                    9 => recommended,
                    10 => 512,
                    _ => 1,
                }
            })
        })
        .expect("fixture query")
    }

    #[test]
    fn preserves_numeric_limits_and_separates_fast_path_support() {
        let caps = fixture(1, 4096, 16);
        assert_eq!(caps.memory_slots, Some(512));
        assert!(caps.cpu_probe_issues(32).is_empty());
        assert!(caps.fixed_backing_prerequisites_present());
        let caps = fixture(0, 4096, 16);
        assert!(caps.cpu_probe_issues(32).is_empty());
        assert!(!caps.fixed_backing_prerequisites_present());
    }

    #[test]
    fn uses_documented_old_host_limits_without_confusing_recommendation_with_maximum() {
        assert!(fixture(1, 0, 8).cpu_probe_issues(9).len() == 1);
        assert!(fixture(1, 0, 0).cpu_probe_issues(4).is_empty());
        assert!(fixture(1, 0, 0).cpu_probe_issues(5).len() == 1);
        assert!(!fixture(1, 32, 8).cpu_probe_issues(0).is_empty());
    }

    #[test]
    fn unknown_api_stops_queries_and_ioctl_errors_remain_errors() {
        let mut calls = 0;
        let caps = collect(|_, _| {
            calls += 1;
            Ok(11)
        })
        .expect("unknown API report");
        assert_eq!(calls, 1);
        assert!(!caps.fixed_backing_prerequisites_present());
        assert!(!caps.cpu_probe_issues(1).is_empty());
        let error = collect(|_, _| Err(io::Error::from_raw_os_error(libc::ENOTTY)))
            .expect_err("failed query must not become an unsupported-capability result");
        assert_eq!(error.raw_os_error(), Some(libc::ENOTTY));
    }
}
