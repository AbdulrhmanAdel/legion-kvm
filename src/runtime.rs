use std::{
    ffi::OsString,
    fs,
    os::unix::process::CommandExt,
    path::{Component, Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub qemu: PathBuf,
    pub qemu_data: PathBuf,
    pub renderer: PathBuf,
    pub virtiofsd: PathBuf,
    pub host_loader: PathBuf,
    pub host_libraries: PathBuf,
    pub kernel: PathBuf,
    pub init: PathBuf,
    pub modules: Vec<PathBuf>,
    pub userspace: PathBuf,
    pub workload_config: PathBuf,
}

pub struct Runtime {
    pub root: PathBuf,
    pub manifest: Manifest,
}

#[derive(Serialize)]
pub struct Check {
    component: &'static str,
    passed: bool,
    detail: String,
}

#[derive(Serialize)]
pub struct Report {
    schema: u32,
    pub passed: bool,
    scope: &'static str,
    checks: Vec<Check>,
}

impl Runtime {
    pub fn load(root: Option<&Path>) -> Result<Self> {
        let root = match root {
            Some(root) => root.to_owned(),
            None => std::env::current_exe()?
                .parent()
                .and_then(Path::parent)
                .context("Legion must be inside the runtime's bin directory")?
                .to_owned(),
        }
        .canonicalize()
        .context("locate runtime directory")?;
        let manifest: Manifest = serde_json::from_slice(
            &fs::read(root.join("runtime.json"))
                .context("runtime.json is missing; extract the complete Legion test bundle")?,
        )?;
        ensure!(manifest.schema == 2, "unsupported runtime manifest schema");
        ensure!(
            !manifest.modules.is_empty(),
            "runtime contains no guest modules"
        );
        let runtime = Self { root, manifest };
        let m = &runtime.manifest;
        for path in [
            &m.qemu,
            &m.qemu_data,
            &m.renderer,
            &m.virtiofsd,
            &m.host_loader,
            &m.host_libraries,
            &m.kernel,
            &m.init,
            &m.userspace,
            &m.workload_config,
        ] {
            runtime.asset(path)?;
        }
        for path in &m.modules {
            runtime.asset(path)?;
        }
        Ok(runtime)
    }

    pub(crate) fn asset(&self, relative: &Path) -> Result<PathBuf> {
        ensure!(
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|p| matches!(p, Component::Normal(_))),
            "runtime asset must be a relative path"
        );
        let path = self
            .root
            .join(relative)
            .canonicalize()
            .with_context(|| format!("missing runtime asset {}", relative.display()))?;
        ensure!(
            path.starts_with(&self.root),
            "runtime asset escapes the bundle"
        );
        Ok(path)
    }

    pub(crate) fn tool(&self, name: &str) -> PathBuf {
        self.root.join("bin").join(name)
    }

    pub fn inspect(&self) -> Report {
        let graphics = crate::graphics::detect();
        let mut checks = Vec::new();
        let mut check = |component, result: Result<String>| {
            checks.push(Check {
                component,
                passed: result.is_ok(),
                detail: match result {
                    Ok(detail) => detail,
                    Err(error) => format!("{error:#}"),
                },
            });
        };
        check(
            "kvm_and_cpu",
            (|| {
                let topology = crate::cpu::host_topology()?;
                let caps = legion_kvm_capabilities::inspect(Path::new("/dev/kvm"))?;
                let issues = caps.cpu_probe_issues(topology.qemu_slots()?);
                ensure!(issues.is_empty(), "{}", issues.join("; "));
                Ok(format!(
                    "{} cores / {} threads",
                    topology.physical_cores, topology.logical_cpus
                ))
            })(),
        );
        check("graphics", graphics.map(|graphics| format!("{graphics:?}")));
        check(
            "desktop",
            (|| {
                let directory =
                    std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is missing")?;
                let display = std::env::var_os("WAYLAND_DISPLAY")
                    .context("a Wayland desktop is required for this alpha")?;
                let path = PathBuf::from(directory).join(display);
                use std::os::unix::fs::FileTypeExt;
                ensure!(
                    path.metadata()?.file_type().is_socket(),
                    "Wayland display is not a socket"
                );
                Ok("Wayland desktop is available".into())
            })(),
        );
        for (name, flag) in [
            ("legion-qemu", "--version"),
            ("legion-virtiofsd", "--version"),
        ] {
            check(
                name,
                (|| {
                    let output = Command::new(self.tool(name)).arg(flag).output()?;
                    ensure!(
                        output.status.success(),
                        "{name} failed ({}): {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr)
                    );
                    Ok(String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .next()
                        .unwrap_or("available")
                        .to_owned())
                })(),
            );
        }
        Report {
            schema: 1,
            passed: checks.iter().all(|c| c.passed),
            scope: "Host prerequisites only; use doctor --graphics to test Venus in the guest",
            checks,
        }
    }
}

/// Executed through bundle-local symlinks to the same Rust launcher binary.
pub fn exec_tool(kind: &str, args: &[OsString]) -> Result<()> {
    let runtime = Runtime::load(None)?;
    let m = &runtime.manifest;
    let asset = match kind {
        "legion-qemu" => &m.qemu,
        "legion-virtiofsd" => &m.virtiofsd,
        "legion-render-server" => &m.renderer,
        _ => bail!("unknown runtime tool"),
    };
    let binary = runtime.asset(asset)?;
    let mut command = Command::new(runtime.asset(&m.host_loader)?);
    command
        .arg("--library-path")
        .arg(runtime.asset(&m.host_libraries)?)
        .arg(&binary);
    if kind == "legion-qemu" {
        command.env("LEGION_VENUS_SYNC_FD", "1");
        if std::env::var_os("LEGION_VENUS_RING_SPIN_US").is_none() {
            command.env("LEGION_VENUS_RING_SPIN_US", "50");
        }
        command.env(
            "RENDER_SERVER_EXEC_PATH",
            runtime.tool("legion-render-server"),
        );
        let plugins: Vec<_> = [
            "/usr/lib/gbm",
            "/usr/lib/x86_64-linux-gnu/gbm",
            "/usr/lib64/gbm",
        ]
        .into_iter()
        .map(Path::new)
        .filter(|path| path.is_dir())
        .collect();
        if !plugins.is_empty() && std::env::var_os("GBM_BACKENDS_PATH").is_none() {
            command.env("GBM_BACKENDS_PATH", std::env::join_paths(plugins)?);
        }
        command.arg("-L").arg(runtime.asset(&m.qemu_data)?);
    }
    if kind == "legion-render-server" {
        command
            .env("VK_LAYER_PATH", runtime.asset(Path::new("host/vulkan"))?)
            .env("VK_INSTANCE_LAYERS", "VK_LAYER_MESA_device_select")
            .env("VK_LOADER_LAYERS_DISABLE", "~implicit~")
            .env("MESA_VK_DEVICE_SELECT_FORCE_DEFAULT_DEVICE", "1")
            .env_remove("MESA_VK_DEVICE_SELECT")
            .env(
                "DRI_PRIME",
                std::env::var_os("LEGION_VENUS_DRI_PRIME")
                    .context("Venus host GPU selection is missing")?,
            );
    }
    command
        .args(args)
        .env_remove("LD_PRELOAD")
        .env_remove("LD_LIBRARY_PATH");
    Err(command.exec()).with_context(|| format!("execute runtime tool {kind}"))
}

pub fn doctor(args: &[OsString]) -> Result<()> {
    let root = match args {
        [] => None,
        [flag, path] if flag == "--runtime" => Some(Path::new(path)),
        _ => bail!("expected runtime-doctor [--runtime <bundle directory>]"),
    };
    let report = Runtime::load(root)?.inspect();
    println!("{}", serde_json::to_string_pretty(&report)?);
    ensure!(report.passed, "runtime prerequisites failed");
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/runtime.rs"]
mod tests;
