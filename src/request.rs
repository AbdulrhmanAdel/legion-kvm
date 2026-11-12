use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::cpu;

#[derive(Debug, Serialize)]
pub struct LaunchRequest {
    pub schema: u32,
    /// Unix arguments can contain non-UTF-8 bytes.
    pub argv_bytes: Vec<Vec<u8>>,
    pub cwd_bytes: Vec<u8>,
    pub cpu_policy: cpu::CpuPolicy,
    pub game_launch_available: bool,
    pub pending: [&'static str; 4],
}

impl LaunchRequest {
    pub fn capture(argv: &[OsString]) -> Result<Self> {
        ensure!(
            !argv.is_empty() && !argv[0].is_empty(),
            "launcher command is empty"
        );
        Ok(Self {
            schema: 1,
            argv_bytes: argv.iter().map(|arg| arg.as_bytes().to_vec()).collect(),
            cwd_bytes: std::env::current_dir()?.as_os_str().as_bytes().to_vec(),
            cpu_policy: cpu::TARGET,
            game_launch_available: false,
            pending: [
                "complete CPU profile",
                "shared GPU transport, matching guest driver and hardware Vulkan",
                "local desktop presentation, input and audio",
                "guest runtime, path mounts and original launcher environment",
            ],
        })
    }
}
