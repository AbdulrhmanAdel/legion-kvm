//! Startup feedback only; no timers, windows or per-frame logging remain after the first forwarded image.
use std::{
    fs::{self, File},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::process::OwnedChild;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Stage {
    Preparing,
    Graphics,
    Booting,
    GuestReady,
    CheckingVulkan,
    StartingDisplay,
    PreparingRuntime,
    StartingApplication,
    DisplayReady,
    Complete,
    Failed,
}

impl Stage {
    fn message(self) -> &'static str {
        match self {
            Self::Preparing => "Preparing the virtual machine…",
            Self::Graphics => "Starting graphics services…",
            Self::Booting => "Starting the virtual machine…",
            Self::GuestReady => "Virtual machine ready; preparing the application…",
            Self::CheckingVulkan => "Checking guest Vulkan…",
            Self::StartingDisplay => "Starting the guest display…",
            Self::PreparingRuntime => "Preparing Proton…",
            Self::StartingApplication => "Starting the application; waiting for its window…",
            Self::DisplayReady => "Application rendering started",
            Self::Complete => "Session finished",
            Self::Failed => "Launch failed; see the session logs",
        }
    }
}

pub(crate) struct Progress {
    start: Instant,
    stage: Option<Stage>,
    log: Option<File>,
    dialog: Option<OwnedChild>,
    frames: PathBuf,
    last_poll: Instant,
    quiet: bool,
}

impl Progress {
    pub(crate) fn new(output: &Path, desktop: bool) -> Self {
        let mode = std::env::var("LEGION_STARTUP_UI").unwrap_or_else(|_| "auto".into());
        let show_window = desktop
            && (mode == "window"
                || (mode == "auto"
                    && !io::stderr().is_terminal()
                    && (std::env::var_os("WAYLAND_DISPLAY").is_some()
                        || std::env::var_os("DISPLAY").is_some())));
        let dialog = show_window
            .then(|| {
                Command::new("zenity")
                    .args([
                        "--progress",
                        "--pulsate",
                        "--auto-close",
                        "--no-cancel",
                        "--title=Legion",
                        "--text=Preparing the virtual machine…",
                        "--width=440",
                    ])
                    .env_remove("LD_PRELOAD")
                    .env_remove("LD_LIBRARY_PATH")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map(OwnedChild)
                    .ok()
            })
            .flatten();
        let mut progress = Self {
            start: Instant::now(),
            stage: None,
            log: File::create(output.join("startup.jsonl")).ok(),
            dialog,
            frames: output.join("frames"),
            last_poll: Instant::now(),
            quiet: mode == "off",
        };
        progress.set(Stage::Preparing);
        progress
    }

    pub(crate) fn set(&mut self, stage: Stage) {
        if self.stage.is_some_and(|current| current >= stage) {
            return;
        }
        self.stage = Some(stage);
        if let Some(log) = &mut self.log {
            let entry = serde_json::json!({"schema":1,"elapsed_ms":self.start.elapsed().as_millis(),
                "stage":stage,"message":stage.message()});
            let _ = writeln!(log, "{entry}");
        }
        if !self.quiet {
            // Keep stdout available for the launcher's machine-readable result.
            let _ = writeln!(io::stderr(), "legion: {}", stage.message());
        }
        if stage >= Stage::DisplayReady {
            self.dialog.take();
        } else if let Some(dialog) = &mut self.dialog {
            if let Some(input) = &mut dialog.0.stdin {
                let _ = writeln!(input, "# {}", stage.message());
            }
            // Closing the progress window never terminates the application.
            if dialog.0.try_wait().is_ok_and(|status| status.is_some()) {
                self.dialog.take();
            }
        }
    }

    pub(crate) fn poll(&mut self) {
        if self.stage.is_some_and(|stage| stage >= Stage::DisplayReady)
            || self.last_poll.elapsed() < Duration::from_millis(250)
        {
            return;
        }
        self.last_poll = Instant::now();
        let status = self.frames.join("startup-status.json");
        if status.metadata().is_ok_and(|m| m.len() <= 1024)
            && let Ok(bytes) = fs::read(status)
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && value["schema"] == 1
        {
            match value["stage"].as_str() {
                Some("preparing-runtime") => self.set(Stage::PreparingRuntime),
                Some("starting-display") => self.set(Stage::StartingDisplay),
                Some("starting-application") => self.set(Stage::StartingApplication),
                _ => {}
            }
        }
        if self.stage.is_none_or(|stage| stage < Stage::CheckingVulkan)
            && let Ok(bytes) = fs::read(self.frames.join("guest-stage.json"))
            && bytes.len() <= 4096
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && value["stage"] == "vulkaninfo"
        {
            self.set(Stage::CheckingVulkan);
        }
        // The existing diagnostic proves the first GPU image was acquired by the forwarding path.
        if self.frames.join("vm-forward-first.rgba").is_file()
            || self.frames.join("gpu-frame-ready").is_file()
            || self.frames.join("kms-first-frame.json").is_file()
        {
            self.set(Stage::DisplayReady);
        }
    }

    pub(crate) fn rendering_started(&self) -> bool {
        self.stage.is_some_and(|stage| stage >= Stage::DisplayReady)
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if self.stage.is_none_or(|stage| stage < Stage::Complete) {
            self.set(Stage::Failed);
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/startup.rs"]
mod tests;
