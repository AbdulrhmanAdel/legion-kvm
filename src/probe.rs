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
mod topology_tests {
    use super::*;

    #[test]
    #[ignore = "requires LEGION_TEST_QEMU, LEGION_TEST_KERNEL, LEGION_TEST_INIT and accessible KVM"]
    fn mixed_smt_layout_boots_and_matches_guest_online_siblings() -> Result<()> {
        use legion_cpu_topology::Core;
        let temporary = tempfile::tempdir()?;
        let records = Topology {
            logical_cpus: 6,
            physical_cores: 4,
            sockets: 1,
            cores_per_socket: 4,
            threads_per_core: 2,
            cpu_ids: (0..6).collect(),
            cores: vec![
                Core {
                    package: 0,
                    die: 0,
                    core: 0,
                    cpu_ids: vec![0, 1],
                },
                Core {
                    package: 0,
                    die: 0,
                    core: 1,
                    cpu_ids: vec![2, 3],
                },
                Core {
                    package: 0,
                    die: 0,
                    core: 2,
                    cpu_ids: vec![4],
                },
                Core {
                    package: 0,
                    die: 0,
                    core: 3,
                    cpu_ids: vec![5],
                },
            ],
        };
        let env_path = |name| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .with_context(|| format!("set {name}"))
        };
        let mut single_first = records.clone();
        for core in &mut single_first.cores {
            core.core = 3 - core.core;
        }
        let mut reports = Vec::new();
        for (name, topology) in [
            ("smt-first", records),
            ("single-first", single_first),
            ("host-control", cpu::host_topology()?),
        ] {
            let report = run_with_topology(
                &Config {
                    qemu: env_path("LEGION_TEST_QEMU")?,
                    kernel: env_path("LEGION_TEST_KERNEL")?,
                    init: env_path("LEGION_TEST_INIT")?,
                    output: temporary.path().join(name),
                    machine: Machine::Q35,
                    timeout: Duration::from_secs(30),
                },
                || Ok(topology),
            )?;
            let passed = report.passed;
            reports.push(serde_json::json!({"case":name,"report":report}));
            if let Some(destination) = std::env::var_os("LEGION_TEST_REPORT") {
                fs::write(destination, serde_json::to_vec_pretty(&reports)?)?;
            }
            ensure!(passed, "guest layout {name} failed: {reports:?}");
        }
        Ok(())
    }

    #[test]
    fn guest_verification_checks_cores_and_threads_not_just_vcpu_count() {
        let expected = Topology {
            logical_cpus: 16,
            physical_cores: 8,
            sockets: 1,
            cores_per_socket: 8,
            threads_per_core: 2,
            cpu_ids: (0..16).collect(),
            cores: Vec::new(),
        };
        let mut report = GuestReport {
            schema: 2,
            passed: true,
            xcr0: Some(7),
            avx512_sigill: true,
            leaf0: [0; 4],
            leaf1: [0; 4],
            leaf7: [0; 4],
            leafd0: [7, 0, 0, 0],
            topology: Some(expected.clone()),
            topology_error: None,
        };
        assert!(report.verify(&expected).is_ok());
        report.topology = Some(Topology {
            physical_cores: 16,
            cores_per_socket: 16,
            threads_per_core: 1,
            ..expected.clone()
        });
        assert!(report.verify(&expected).is_err());
        report.topology = None;
        assert!(report.verify(&expected).is_err());
    }
}

#[cfg(test)]
mod drm_tests {
    use super::*;

    #[test]
    #[ignore = "requires LEGION_DRM_TEST_QEMU, KERNEL, INIT, MODULES and accessible KVM"]
    fn virtio_gpu_guest_modules_bind_and_report_capsets() -> Result<()> {
        let env_path = |name| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .with_context(|| format!("set {name}"))
        };
        let modules: Vec<PathBuf> =
            serde_json::from_str(&std::env::var("LEGION_DRM_TEST_MODULES")?)?;
        let modules = modules
            .iter()
            .map(fs::read)
            .collect::<std::io::Result<Vec<_>>>()?;
        ensure!(!modules.is_empty(), "guest modules are required");
        let directory = tempfile::tempdir()?;
        let initrd = directory.path().join("initramfs.cpio");
        archive::write_with_modules(
            File::create(&initrd)?,
            &fs::read(env_path("LEGION_DRM_TEST_INIT")?)?,
            &modules,
        )?;
        let console = directory.path().join("console.log");
        let stderr = directory.path().join("qemu.log");
        let topology = cpu::host_topology()?;
        let model = cpu::boot_cpu()?;
        let mut command = Command::new(env_path("LEGION_DRM_TEST_QEMU")?);
        command.arg("-L").arg(
            std::env::var_os("LEGION_DRM_TEST_QEMU_DATA")
                .unwrap_or_else(|| "/usr/share/qemu".into()),
        );
        command
            .args([
                "-machine",
                "q35,accel=kvm,smm=off",
                "-cpu",
                &model,
                "-m",
                "512",
            ])
            .args(topology.qemu_args(&model)?)
            .args([
                "-nodefaults",
                "-no-reboot",
                "-display",
                "none",
                "-monitor",
                "none",
                "-serial",
                "stdio",
                "-nic",
                "none",
                "-device",
                "virtio-gpu-pci",
            ])
            .arg("-kernel")
            .arg(env_path("LEGION_DRM_TEST_KERNEL")?)
            .arg("-initrd")
            .arg(initrd)
            .args([
                "-append",
                "console=ttyS0 rdinit=/init quiet panic=-1 legion.drm_probe=1",
            ])
            .env_remove("LD_PRELOAD")
            .stdin(Stdio::null())
            .stdout(File::create(&console)?)
            .stderr(File::create(&stderr)?);
        let mut child = OwnedChild(command.spawn()?);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let text = fs::read_to_string(&console)?;
            if let Some(line) = text
                .lines()
                .find_map(|line| line.strip_prefix("LEGION_DRM_PROBE "))
            {
                let driver: serde_json::Value = serde_json::from_str(line)?;
                let cpu = read_report(&console)?.context("CPU report missing")?;
                child.stop()?;
                if let Some(path) = std::env::var_os("LEGION_DRM_TEST_REPORT") {
                    fs::write(
                        path,
                        serde_json::to_vec_pretty(&serde_json::json!({
                            "cpu":cpu,"driver":driver,"console":text,"qemu_stderr":fs::read_to_string(&stderr)?
                        }))?,
                    )?;
                }
                cpu.verify(&topology)?;
                ensure!(
                    driver["passed"] == true,
                    "virtio-GPU module probe failed: {driver}; guest console: {text}"
                );
                ensure!(
                    driver["hardware_rendering_verified"] == false,
                    "driver probe must not claim rendering"
                );
                return Ok(());
            }
            ensure!(
                child.0.try_wait()?.is_none(),
                "DRM test guest exited: {}",
                fs::read_to_string(&stderr)?
            );
            ensure!(
                Instant::now() < deadline,
                "DRM test guest timed out: {text}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
