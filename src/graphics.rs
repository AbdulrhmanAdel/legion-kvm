//! Select graphics by the loaded DRM driver, with an optional explicit node.
use crate::doctor::{self, RenderDevice};
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Graphics {
    Nvidia,
    Amd(PathBuf),
}

pub(crate) fn select(devices: &[RenderDevice], requested: Option<&Path>) -> Result<Graphics> {
    let eligible = |device: &&RenderDevice| {
        device.read_write_access && matches!(device.driver.as_deref(), Some("nvidia" | "amdgpu"))
    };
    let device = if let Some(node) = requested {
        devices
            .iter()
            .filter(eligible)
            .find(|device| device.node == node)
            .context("selected render node is unavailable or unsupported")?
    } else {
        // Preserve the established NVIDIA default on mixed systems.
        devices
            .iter()
            .filter(eligible)
            .find(|device| device.driver.as_deref() == Some("nvidia"))
            .or_else(|| devices.iter().find(eligible))
            .context("no accessible NVIDIA or AMD graphics device")?
    };
    Ok(if device.driver.as_deref() == Some("amdgpu") {
        Graphics::Amd(device.node.clone())
    } else {
        Graphics::Nvidia
    })
}

pub(crate) fn detect() -> Result<Graphics> {
    let node = std::env::var_os("LEGION_RENDER_NODE")
        .map(std::fs::canonicalize)
        .transpose()?;
    select(&doctor::render_devices()?, node.as_deref())
}

pub(crate) fn require_amd(selection: &Graphics) -> Result<&Path> {
    ensure!(
        matches!(selection, Graphics::Amd(_)),
        "this rendering benchmark requires an AMD render node"
    );
    match selection {
        Graphics::Amd(node) => Ok(node),
        Graphics::Nvidia => unreachable!("checked above"),
    }
}

#[cfg(test)]
#[path = "../tests/unit/graphics.rs"]
mod tests;
