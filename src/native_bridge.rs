//! Development controls for readback and DMA-BUF forwarding outside KVM.
use std::ffi::OsStr;
use std::fs::{self, File};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::{
    presentation::{Config, Session},
    process::{CoreDumpLimit, OwnedChild},
};

pub fn run(
    config: &Config,
    userspace: &Path,
    output: &Path,
    cpus: Option<&OsStr>,
) -> Result<Value> {
    config.validate()?;
    if let Some(cpus) = cpus {
        ensure!(
            cpus.to_str().is_some_and(|text| !text.is_empty()
                && text
                    .bytes()
                    .all(|c| c.is_ascii_digit() || c == b',' || c == b'-')),
            "native CPU list must contain CPU numbers, commas or ranges"
        );
    }
    ensure!(
        config.application,
        "native bridge requires an application renderer"
    );
    ensure!(!output.as_os_str().is_empty(), "--output is required");
    let userspace = fs::canonicalize(userspace).context("locate baseline userspace")?;
    let loader = userspace.join("lib/x86_64-linux-gnu/ld-linux-x86-64.so.2");
    ensure!(
        loader.is_file() && userspace.join("bin/vkcube").is_file(),
        "userspace must contain the baseline loader and vkcube"
    );
    let compositor = fs::canonicalize(&config.guest)?;
    let _core_limit = CoreDumpLimit::disabled()?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(output)
        .context("create native-control output (must not exist)")?;
    let output = fs::canonicalize(output)?;
    fs::create_dir(output.join("frames"))?;
    let scratch = tempfile::Builder::new()
        .prefix("legion-native-")
        .tempdir_in("/dev/shm")?;
    let mut session = Session::start_host(config, &output)?;
    let log = File::create(output.join("compositor.log"))?;
    let layer_path = output.join("layers");
    fs::create_dir(&layer_path)?;
    if config.uncapped {
        let mut manifest: Value = serde_json::from_slice(&fs::read(
            userspace.join("share/vulkan/implicit_layer.d/MangoHud.x86_64.json"),
        )?)?;
        manifest["layer"]["library_path"] =
            json!(userspace.join("lib/x86_64-linux-gnu/mangohud/libMangoHud.so"));
        serde_json::to_writer(File::create(layer_path.join("MangoHud.json"))?, &manifest)?;
    }
    let mut command = if let Some(cpus) = cpus {
        let mut command = Command::new("taskset");
        command.arg("--cpu-list").arg(cpus).arg(&loader);
        command
    } else {
        Command::new(&loader)
    };
    command
        .arg("--library-path")
        .arg(std::env::join_paths([
            userspace.join("lib"),
            userspace.join("lib/x86_64-linux-gnu"),
        ])?)
        .arg(compositor)
        .arg("--native")
        .arg(session.channel_path()?)
        .arg(output.join("frames"))
        .arg(&userspace)
        .arg(scratch.path())
        .env_remove("LD_PRELOAD")
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("DISPLAY")
        .env("XDG_RUNTIME_DIR", scratch.path())
        .env(
            "VK_DRIVER_FILES",
            userspace.join("share/vulkan/icd.d/nvidia_icd.json"),
        )
        .env(
            "__EGL_VENDOR_LIBRARY_FILENAMES",
            userspace.join("share/glvnd/egl_vendor.d/10_nvidia.json"),
        )
        .env("VK_LOADER_LAYERS_DISABLE", "~implicit~")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if config.uncapped {
        command
            .env("VK_LAYER_PATH", layer_path)
            .env("VK_INSTANCE_LAYERS", "VK_LAYER_MANGOHUD_overlay_x86_64");
    }
    let mut child = OwnedChild(command.spawn().context("start native compositor")?);
    let start = Instant::now();
    let outcome = (|| -> Result<Value> {
        loop {
            session.check()?;
            if let Some(status) = child.0.try_wait()? {
                ensure!(
                    status.success(),
                    "native compositor exited {status}; inspect compositor.log"
                );
                break;
            }
            ensure!(
                start.elapsed() < Duration::from_secs(config.seconds + 30),
                "native bridge timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let presentation = session.finish()?;
        let text = fs::read_to_string(output.join("compositor.log"))?;
        let compositor: Value = text
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<Value>(line).ok())
            .context("missing compositor result")?;
        ensure!(
            compositor["passed"] == true && compositor["native_control"] == true,
            "native compositor verification failed"
        );
        Ok(
            json!({"schema":1,"passed":true,"mode":"native_bridge","virtual_machine":false,"cpu_affinity":cpus.map(OsStr::to_string_lossy),"uncapped":config.uncapped,"presentation":presentation,"compositor":compositor,"elapsed_ms":start.elapsed().as_millis()}),
        )
    })();
    let report = match &outcome {
        Ok(report) => report.clone(),
        Err(error) => {
            json!({"schema":1,"passed":false,"mode":"native_bridge","error":format!("{error:#}")})
        }
    };
    serde_json::to_writer_pretty(File::create(output.join("report.json"))?, &report)?;
    outcome
}

pub struct ForwardConfig {
    pub compositor: PathBuf,
    pub userspace: PathBuf,
    pub output: PathBuf,
    pub width: u32,
    pub height: u32,
    pub seconds: u64,
    pub acquire_barrier: bool,
}

/// Native control for the fast display path.
pub fn run_forward(config: &ForwardConfig) -> Result<Value> {
    ensure!(
        (15..=30).contains(&config.seconds),
        "forwarding benchmark duration must be 15..=30 seconds"
    );
    ensure!(
        (1..=3840).contains(&config.width) && (1..=2160).contains(&config.height),
        "invalid forwarding dimensions"
    );
    ensure!(
        !config.output.as_os_str().is_empty(),
        "--output is required"
    );
    let compositor =
        fs::canonicalize(&config.compositor).context("locate forwarding compositor")?;
    let userspace = fs::canonicalize(&config.userspace).context("locate baseline userspace")?;
    let loader = userspace.join("lib/x86_64-linux-gnu/ld-linux-x86-64.so.2");
    ensure!(
        loader.is_file() && userspace.join("bin/vkcube").is_file(),
        "userspace requires baseline loader and vkcube"
    );
    let mut manifest: Value = serde_json::from_slice(&fs::read(
        userspace.join("share/vulkan/implicit_layer.d/MangoHud.x86_64.json"),
    )?)?;
    manifest["layer"]["library_path"] =
        json!(userspace.join("lib/x86_64-linux-gnu/mangohud/libMangoHud.so"));
    let _core_limit = CoreDumpLimit::disabled()?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&config.output)
        .context("create forwarding output (must not exist)")?;
    let output = fs::canonicalize(&config.output)?;
    fs::create_dir(output.join("frames"))?;
    let layers = output.join("layers");
    fs::create_dir(&layers)?;
    serde_json::to_writer(File::create(layers.join("MangoHud.json"))?, &manifest)?;
    let scratch = tempfile::Builder::new()
        .prefix("legion-forward-")
        .tempdir_in("/dev/shm")?;
    let log = File::create(output.join("compositor.log"))?;
    let mut command = Command::new(loader);
    command
        .arg("--library-path")
        .arg(std::env::join_paths([
            userspace.join("lib"),
            userspace.join("lib/x86_64-linux-gnu"),
        ])?)
        .arg(compositor)
        .arg("--native-forward")
        .arg(output.join("frames"))
        .arg(&userspace)
        .arg(scratch.path())
        .arg(config.width.to_string())
        .arg(config.height.to_string())
        .arg(config.seconds.to_string())
        .arg(config.acquire_barrier.to_string())
        .env_remove("LD_PRELOAD")
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("DISPLAY")
        // These selectors belong to the local NVIDIA test runtime, not the common Wayland/DMA-BUF interface or eventual automatic selection.
        .env(
            "VK_DRIVER_FILES",
            userspace.join("share/vulkan/icd.d/nvidia_icd.json"),
        )
        .env(
            "__EGL_VENDOR_LIBRARY_FILENAMES",
            userspace.join("share/glvnd/egl_vendor.d/10_nvidia.json"),
        )
        .env("VK_LAYER_PATH", layers)
        .env("VK_INSTANCE_LAYERS", "VK_LAYER_MANGOHUD_overlay_x86_64")
        .env("VK_LOADER_LAYERS_DISABLE", "~implicit~")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let mut child = ForwardChild {
        child: OwnedChild(
            command
                .spawn()
                .context("start native forwarding compositor")?,
        ),
        reaped: false,
    };
    let start = Instant::now();
    let outcome = (|| -> Result<Value> {
        let status = child.wait_until(Duration::from_secs(config.seconds + 30))?;
        ensure!(
            status.success(),
            "forwarding compositor exited {status}; inspect compositor.log"
        );
        let text = fs::read_to_string(output.join("compositor.log"))?;
        let compositor: Value = text
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str(line).ok())
            .context("missing forwarding report")?;
        ensure!(
            compositor["passed"] == true
                && compositor["cpu_pixel_transport"] == false
                && compositor["animation_verified"] == true,
            "forwarding verification failed"
        );
        Ok(
            json!({"schema":1,"passed":true,"mode":"native_dmabuf_forward","virtual_machine":false,"compositor":compositor,"elapsed_ms":start.elapsed().as_millis()}),
        )
    })();
    let report = match &outcome {
        Ok(report) => report.clone(),
        Err(error) => {
            json!({"schema":1,"passed":false,"mode":"native_dmabuf_forward","error":format!("{error:#}")})
        }
    };
    serde_json::to_writer_pretty(File::create(output.join("report.json"))?, &report)?;
    outcome
}

// The dedicated group belongs only to this new control and its descendants.
struct ForwardChild {
    child: OwnedChild,
    reaped: bool,
}
impl ForwardChild {
    fn wait_until(&mut self, timeout: Duration) -> Result<std::process::ExitStatus> {
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
        let start = Instant::now();
        loop {
            if waitid(
                WaitId::Pid(Pid::from_child(&self.child.0)),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            )?
            .is_some()
            {
                self.stop_group();
                let status = self.child.0.wait()?;
                self.reaped = true;
                return Ok(status);
            }
            ensure!(start.elapsed() < timeout, "native forwarding timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn stop_group(&self) {
        let _ = rustix::process::kill_process_group(
            rustix::process::Pid::from_child(&self.child.0),
            rustix::process::Signal::KILL,
        );
    }
}
impl Drop for ForwardChild {
    fn drop(&mut self) {
        if !self.reaped {
            self.stop_group();
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/native_bridge.rs"]
mod tests;
