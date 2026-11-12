use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use legion_display_channel::Channel;
use serde_json::Value;

use crate::process::OwnedChild;

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquireMode {
    Async,
    Blocking,
}

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameTransport {
    Separate,
    Combined,
    Notified,
    Polled,
}

#[derive(Clone, Copy, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FramePacing {
    Unpaced,
    #[default]
    Callbacks,
}

pub struct Config {
    pub guest: PathBuf,
    pub presenter: PathBuf,
    pub seconds: u64,
    pub width: u32,
    pub height: u32,
    pub self_test: bool,
    pub application: bool,
    pub uncapped: bool,
    pub gpu_forward: bool,
    pub acquire_mode: Option<AcquireMode>,
    pub frame_transport: Option<FrameTransport>,
    pub frame_pacing: Option<FramePacing>,
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.frame_pacing.is_none() || self.gpu_forward,
            "--frame-pacing requires --gpu-forward true"
        );
        ensure!(
            self.frame_transport.is_none() || self.gpu_forward,
            "--frame-transport requires --gpu-forward true"
        );
        ensure!(
            self.acquire_mode.is_none() || self.gpu_forward,
            "--acquire-mode requires --gpu-forward true"
        );
        ensure!(
            !self.gpu_forward
                || (self.application && self.uncapped && !self.self_test && self.seconds >= 15),
            "GPU forwarding currently requires an uncapped application, --self-test false and at least 15 seconds"
        );
        ensure!(
            self.guest.is_file(),
            "--guest-display must name the guest executable"
        );
        ensure!(
            self.gpu_forward || self.presenter.is_file(),
            "--presenter must name the host executable"
        );
        ensure!(
            (1..=30).contains(&self.seconds),
            "presentation duration must be 1..=30 seconds"
        );
        ensure!(
            !self.application || !self.self_test || self.seconds >= 3,
            "application input self-test requires at least 3 seconds"
        );
        ensure!(
            !self.uncapped || (self.application && !self.self_test),
            "uncapped benchmarks require an application and --self-test false"
        );
        ensure!(
            !self.uncapped || self.seconds >= 12,
            "uncapped benchmark logging requires at least 12 seconds"
        );
        ensure!(
            (1..=3840).contains(&self.width) && (1..=2160).contains(&self.height),
            "presentation dimensions exceed 3840x2160"
        );
        Ok(())
    }
}

pub struct Session {
    channel: Channel,
    viewer: OwnedChild,
    report_path: PathBuf,
    self_test: bool,
}
impl Session {
    pub fn start(config: &Config, output: &Path, qemu: &mut Command) -> Result<Self> {
        let session = Self::start_host(config, output)?;
        let path = session.channel.proc_path()?;
        qemu.arg("-object")
            .arg(format!(
                "memory-backend-file,id=display-mem,mem-path={},size={},share=on",
                path.display(),
                session.channel.mapping_len()
            ))
            .args(["-device", "ivshmem-plain,id=display-shm,memdev=display-mem"]);
        Ok(session)
    }

    pub fn start_host(config: &Config, output: &Path) -> Result<Self> {
        config.validate()?;
        serde_json::to_writer(
            File::create(output.join("frames/presentation-config.json"))?,
            &serde_json::json!({"schema":1,"uncapped":config.uncapped}),
        )?;
        let channel = Channel::create(config.width, config.height)?;
        let path = channel.proc_path()?;
        let report_path = output.join("presentation.json");
        let log = File::create(output.join("presenter.log"))?;
        let mut command = Command::new(&config.presenter);
        command
            .arg(path)
            .arg(&report_path)
            .arg(config.seconds.to_string())
            .arg(config.self_test.to_string())
            .arg(if config.application {
                "application"
            } else {
                "color"
            })
            .arg(config.uncapped.to_string())
            .env_remove("LD_PRELOAD")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        if std::env::var_os("SDL_VIDEODRIVER").is_none()
            && std::env::var_os("WAYLAND_DISPLAY").is_some()
        {
            command.env("SDL_VIDEODRIVER", "wayland");
        }
        let viewer = OwnedChild(command.spawn().context("start desktop presenter")?);
        Ok(Self {
            channel,
            viewer,
            report_path,
            self_test: config.self_test,
        })
    }

    pub fn channel_path(&self) -> Result<PathBuf> {
        self.channel.proc_path()
    }

    pub fn check(&mut self) -> Result<()> {
        if self.viewer.0.try_wait()?.is_some() {
            // Let the producer stop and emit its result even if the presenter failed. finish() checks its status after collecting guest proof.
            self.channel.stop();
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<Value> {
        self.channel.stop();
        let status = self.viewer.wait_until(Duration::from_secs(3))?;
        ensure!(status.success(), "desktop presenter exited unsuccessfully");
        let report: Value = serde_json::from_slice(
            &fs::read(&self.report_path).context("read presentation result")?,
        )?;
        ensure!(
            report["schema"] == 1 && report["passed"] == true && report["window_created"] == true,
            "desktop presentation verification failed"
        );
        ensure!(
            report["frames_presented"].as_u64().is_some_and(|n| n != 0),
            "no frames presented"
        );
        if self.self_test {
            ensure!(
                report["input_roundtrip_verified"] == true,
                "input roundtrip was not verified"
            );
        }
        ensure!(
            report["video_driver"] != "dummy" && report["video_driver"] != "offscreen",
            "presentation requires a real desktop display"
        );
        Ok(report)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.channel.stop();
    }
}
