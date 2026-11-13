use std::fs::{self, File};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use legion_cpu_topology::Topology;
use serde::{Deserialize, Serialize};

use crate::{archive, cpu, process::OwnedChild};

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Machine {
    Q35,
    Microvm,
}

impl Machine {
    fn qemu_arg(self) -> &'static str {
        match self {
            // The appliance boots Linux directly and has no SMM firmware.
            Self::Q35 => "q35,accel=kvm,smm=off",
            Self::Microvm => "microvm,accel=kvm",
        }
    }
}

pub struct Config {
    pub qemu: PathBuf,
    pub kernel: PathBuf,
    pub init: PathBuf,
    pub output: PathBuf,
    pub machine: Machine,
    pub timeout: Duration,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GuestReport {
    pub schema: u32,
    pub passed: bool,
    pub xcr0: Option<u64>,
    pub avx512_sigill: bool,
    pub leaf0: [u32; 4],
    pub leaf1: [u32; 4],
    pub leaf7: [u32; 4],
    pub leafd0: [u32; 4],
    pub topology: Option<Topology>,
    pub topology_error: Option<String>,
}

impl GuestReport {
    pub(crate) fn verify(&self, expected: &Topology) -> Result<()> {
        ensure!(
            self.schema == 2,
            "unknown guest CPU report schema; rebuild legion-guest-probe"
        );
        let topology = self.topology.as_ref().with_context(|| {
            format!("guest topology detection failed: {:?}", self.topology_error)
        })?;
        ensure!(
            topology.same_shape(expected),
            "guest CPU topology differs from host: guest {topology:?}, host {expected:?}"
        );
        ensure!(
            self.xcr0 == Some(cpu::TARGET.xcr0),
            "guest XCR0 is {:?}, expected 0x7",
            self.xcr0
        );
        ensure!(
            self.leaf7[1] & (1 << 16) == 0,
            "guest still advertises AVX-512F"
        );
        ensure!(
            self.leafd0[0] == 7 && self.leafd0[3] == 0,
            "guest still advertises additional XCR0 state"
        );
        ensure!(
            self.avx512_sigill,
            "guest AVX-512 instruction did not fault with SIGILL"
        );
        ensure!(self.passed, "guest reported failure");
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct ProbeReport {
    pub schema: u32,
    pub passed: bool,
    pub machine: Machine,
    pub kernel: PathBuf,
    pub init: PathBuf,
    pub boot_cpu: String,
    pub cpu_policy: cpu::CpuPolicy,
    pub complete_profile_verified: bool,
    pub cpu_topology: Topology,
    pub elapsed_ms: u128,
    pub guest: Option<GuestReport>,
    pub error: Option<String>,
}

pub fn run(config: &Config) -> Result<ProbeReport> {
    run_with_topology(config, cpu::host_topology)
}

fn run_with_topology(
    config: &Config,
    detect: impl FnOnce() -> Result<Topology>,
) -> Result<ProbeReport> {
    ensure!(
        !config.kernel.as_os_str().is_empty(),
        "--kernel is required"
    );
    ensure!(!config.init.as_os_str().is_empty(), "--init is required");
    ensure!(
        !config.output.as_os_str().is_empty(),
        "--output is required and must name a new directory"
    );
    let kernel = fs::canonicalize(&config.kernel).context("locate guest kernel")?;
    ensure!(kernel.is_file(), "guest kernel must be a file");
    let init = fs::canonicalize(&config.init).context("locate static guest init")?;
    ensure!(
        init.metadata()?.len() <= 32 * 1024 * 1024,
        "guest init exceeds 32 MiB"
    );
    let init_bytes = fs::read(&init)?;
    validate_static_elf(&init_bytes)?;
    let topology = detect().context("detect CPU topology")?;
    let boot_cpu = cpu::boot_cpu()?;
    ensure!(
        topology.uniform_smt() || matches!(config.machine, Machine::Q35),
        "mixed-SMT CPU layouts require the q35 machine"
    );
    let cpu_args = topology.qemu_args(&boot_cpu)?;

    fs::DirBuilder::new()
        .mode(0o700)
        .create(&config.output)
        .context("create probe directory (existing directories are never overwritten)")?;
    let output = fs::canonicalize(&config.output)?;
    let initramfs = output.join("initramfs.cpio");
    archive::write_initramfs(File::create(&initramfs)?, &init_bytes)?;
    let console = output.join("console.log");
    let stderr = output.join("qemu.log");
    let start = Instant::now();
    let mut command = Command::new(&config.qemu);
    command
        .args(["-machine", config.machine.qemu_arg(), "-cpu", &boot_cpu])
        .args(["-m", "512", "-nodefaults", "-no-reboot"])
        .args(cpu_args)
        .args([
            "-display", "none", "-monitor", "none", "-serial", "stdio", "-nic", "none",
        ])
        .arg("-kernel")
        .arg(&kernel)
        .arg("-initrd")
        .arg(&initramfs)
        .args(["-append", "console=ttyS0 rdinit=/init quiet panic=-1"])
        .env_remove("LD_PRELOAD")
        .stdin(Stdio::null())
        .stdout(File::create(&console)?)
        .stderr(File::create(&stderr)?);

    let outcome = (|| -> Result<GuestReport> {
        let mut child = OwnedChild(command.spawn().context("start QEMU KVM hardware probe")?);
        loop {
            if let Some(report) = read_report(&console)? {
                child.stop()?;
                return Ok(report);
            }
            if let Some(status) = child.0.try_wait()? {
                // Capture a final line even if the child exited between checks.
                if let Some(report) = read_report(&console)? {
                    return Ok(report);
                }
                bail!(
                    "QEMU exited with {status}; inspect {} and {}",
                    console.display(),
                    stderr.display()
                );
            }
            ensure!(
                start.elapsed() < config.timeout,
                "guest timed out; inspect {} and {}",
                console.display(),
                stderr.display()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    })();
    let (guest, error) = match outcome {
        Ok(report) => {
            let error = report
                .verify(&topology)
                .err()
                .map(|error| format!("{error:#}"));
            (Some(report), error)
        }
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let report = ProbeReport {
        schema: 1,
        passed: error.is_none(),
        machine: config.machine,
        kernel,
        init,
        boot_cpu,
        cpu_policy: cpu::TARGET,
        complete_profile_verified: false,
        cpu_topology: topology,
        elapsed_ms: start.elapsed().as_millis(),
        guest,
        error,
    };
    serde_json::to_writer_pretty(File::create(output.join("report.json"))?, &report)?;
    if let Some(error) = &report.error {
        bail!(
            "{error}; result saved to {}",
            output.join("report.json").display()
        );
    }
    Ok(report)
}

pub(crate) fn read_report(console: &Path) -> Result<Option<GuestReport>> {
    ensure!(
        console.metadata()?.len() <= 4 * 1024 * 1024,
        "guest console exceeds 4 MiB"
    );
    let bytes = fs::read(console)?;
    // Ignore boot firmware's terminal control bytes and incomplete final lines.
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if line.last() != Some(&b'\n') {
            continue;
        }
        let Some(json) = line.strip_prefix(b"LEGION_PROBE ") else {
            continue;
        };
        return Ok(Some(
            serde_json::from_slice(json).context("parse guest report")?,
        ));
    }
    Ok(None)
}

pub(crate) fn validate_static_elf(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() >= 64 && bytes.starts_with(b"\x7fELF\x02\x01"),
        "guest init must be a 64-bit little-endian ELF"
    );
    let u16_at = |offset| u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
    ensure!(u16_at(18) == 62, "guest init must target x86-64");
    let phoff = usize::try_from(u64::from_le_bytes(bytes[32..40].try_into()?))?;
    let phsize = usize::from(u16_at(54));
    let phnum = usize::from(u16_at(56));
    ensure!(
        phsize == 56 && phnum != 0,
        "invalid ELF program header table"
    );
    let table_size = phsize
        .checked_mul(phnum)
        .context("ELF table size overflow")?;
    let table_end = phoff
        .checked_add(table_size)
        .context("ELF table offset overflow")?;
    ensure!(
        table_end <= bytes.len(),
        "ELF program headers exceed file length"
    );
    for header in bytes[phoff..table_end].chunks_exact(phsize) {
        let kind = u32::from_le_bytes(header[..4].try_into()?);
        ensure!(
            kind != 3,
            "guest init has an ELF interpreter; build legion-guest-probe for x86_64-unknown-linux-musl"
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/probe_topology_tests.rs"]
mod topology_tests;

#[cfg(test)]
#[path = "../tests/unit/probe_drm_tests.rs"]
mod drm_tests;
